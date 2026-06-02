// Topology graph powered by Cytoscape.js with compound (parent/child) nodes.
//
// Each switch is a parent node containing one small child node per connected
// port. Edges connect port → port, so the arrowhead lands precisely on the
// port it represents — matching the old graphviz table-row layout where each
// edge anchored to a specific port row.
//
// Node styling:
//   .device.managed   → gray fill, hostname label at top of container
//   .device.unmanaged → white fill, hostname label at top
//   .port             → small white box with the port name, inside its parent
// Edge: triangle arrowhead at target end (source SEES target).
//
// Clicking a device (the container or any of its ports) populates a side
// panel with description, role, ip, platform, version, and all adjacencies.

(function () {
  var cy = null;
  var lastData = null;

  // Tracks the most recent ctrl+right-click target for BFS-cycle selection.
  // Repeating ctrl+right-click on the same device expands the BFS radius;
  // clicking a different device resets to depth 1.
  var bfsState = null; // { rootId, depth }

  // Flag set during programmatic multi-select so the per-node "select"
  // handler doesn't fire renderDetail / highlightSiblings for every
  // element in the selection set.
  var suppressSelectHandler = false;

  // localStorage key for persisted node positions. Per-origin → effectively
  // per-browser-per-user-account; explicitly cleared by the Reset button.
  var POS_KEY = "aynmsgui:topology:positions";
  // Cached last topology response. Restored on page load so the graph is
  // visible (with everything initially marked stale) before the first
  // /topology/json fetch completes.
  var DATA_KEY = "aynmsgui:topology:lastData";
  // Set of device ids whose stale state has been acknowledged by the
  // operator. Managed devices that are stale AND not in this set render
  // red ("alarming"); once acknowledged they fade to normal gray. The
  // ack is cleared automatically when a device comes back online so the
  // next disconnect re-alarms.
  var ACK_KEY = "aynmsgui:topology:ackedStale";
  // Set of device ids the operator has flagged as "important" (typically
  // unmanaged devices the operator cares about — e.g., a critical AP or
  // codec). Important devices alarm red on stale, same as managed ones.
  var IMPORTANT_KEY = "aynmsgui:topology:important";

  function loadSavedPositions() {
    try {
      var raw = localStorage.getItem(POS_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch (e) {
      return null;
    }
  }

  function saveCurrentPositions() {
    if (!cy) return;
    var out = {};
    cy.nodes().forEach(function (n) {
      var p = n.position();
      out[n.id()] = { x: p.x, y: p.y };
    });
    try {
      localStorage.setItem(POS_KEY, JSON.stringify(out));
    } catch (e) {
      console.warn("topology: failed to save positions", e);
    }
  }

  function clearSavedPositions() {
    try {
      localStorage.removeItem(POS_KEY);
    } catch (e) {}
  }

  // The "shadow store" accumulates every node/edge id we've ever seen
  // (keyed by id, latest data wins). On every successful fetch the new
  // items are merged in — they're never removed. On page reload we
  // restore the full shadow with everything marked stale, so devices
  // that have gone away keep their gray placeholder until either the
  // backend reports them again or the user clears the history.
  //
  // Stored shape: { nodes: { id: nodeData, ... }, edges: { id: edgeData, ... } }

  function readShadow() {
    try {
      var raw = localStorage.getItem(DATA_KEY);
      var s = raw ? JSON.parse(raw) : { nodes: {}, edges: {} };
      if (!s.nodes) s.nodes = {};
      if (!s.edges) s.edges = {};
      return s;
    } catch (e) {
      return { nodes: {}, edges: {} };
    }
  }

  function updateShadow(data) {
    try {
      var s = readShadow();
      (data.nodes || []).forEach(function (n) {
        if (n && n.id) s.nodes[n.id] = n;
      });
      (data.edges || []).forEach(function (e) {
        if (e && e.id && e.source && e.target) s.edges[e.id] = e;
      });
      localStorage.setItem(DATA_KEY, JSON.stringify(s));
    } catch (e) {
      console.warn("topology: failed to update shadow", e);
    }
  }

  function clearShadow() {
    try { localStorage.removeItem(DATA_KEY); } catch (e) {}
  }

  function loadAcks() {
    try {
      var raw = localStorage.getItem(ACK_KEY);
      return raw ? new Set(JSON.parse(raw)) : new Set();
    } catch (e) {
      return new Set();
    }
  }
  function saveAcks(set) {
    try {
      localStorage.setItem(ACK_KEY, JSON.stringify(Array.from(set)));
    } catch (e) {}
  }
  function ackDevice(id) {
    var s = loadAcks();
    s.add(id);
    saveAcks(s);
    var node = cy.getElementById(id);
    if (node.length) node.removeClass("stale-unacked");
    // Re-render the detail panel to drop the Acknowledge button.
    if (node.length) renderDetail(node);
  }
  function unackDevice(id) {
    var s = loadAcks();
    if (s.has(id)) {
      s.delete(id);
      saveAcks(s);
    }
  }

  function loadImportant() {
    try {
      var raw = localStorage.getItem(IMPORTANT_KEY);
      return raw ? new Set(JSON.parse(raw)) : new Set();
    } catch (e) {
      return new Set();
    }
  }
  function saveImportant(set) {
    try {
      localStorage.setItem(IMPORTANT_KEY, JSON.stringify(Array.from(set)));
    } catch (e) {}
  }
  function markImportant(id) {
    var s = loadImportant();
    s.add(id);
    saveImportant(s);
    var node = cy.getElementById(id);
    if (node.length) node.addClass("important");
    refreshStaleAlarms();
    if (node.length) renderDetail(node);
  }
  function unmarkImportant(id) {
    var s = loadImportant();
    if (s.delete(id)) saveImportant(s);
    var node = cy.getElementById(id);
    if (node.length) node.removeClass("important");
    refreshStaleAlarms();
    if (node.length) renderDetail(node);
  }

  // Walk every device and refresh both its .important marker (from the
  // saved set) and its .stale-unacked alarm marker. A device alarms
  // when stale AND (managed OR important) AND not already acked.
  function refreshStaleAlarms() {
    if (!cy) return;
    var acks = loadAcks();
    var important = loadImportant();
    cy.nodes(".device").forEach(function (d) {
      var stale = d.hasClass("stale");
      var managed = !!d.data("managed");
      var imp = important.has(d.id());
      if (imp) d.addClass("important");
      else d.removeClass("important");
      var alarmed = stale && (managed || imp) && !acks.has(d.id());
      if (alarmed) d.addClass("stale-unacked");
      else d.removeClass("stale-unacked");
    });
  }

  // Remove a single id from the shadow (kind = "nodes" or "edges").
  function removeFromShadow(kind, id) {
    try {
      var raw = localStorage.getItem(DATA_KEY);
      if (!raw) return;
      var s = JSON.parse(raw);
      if (s[kind] && s[kind][id] !== undefined) {
        delete s[kind][id];
        localStorage.setItem(DATA_KEY, JSON.stringify(s));
      }
    } catch (e) {}
  }

  // Drop every shadow edge whose source or target is this device id.
  // (Shadow edges store device ids in source/target, not port ids.)
  function purgeShadowEdgesForDevice(deviceId) {
    try {
      var raw = localStorage.getItem(DATA_KEY);
      if (!raw) return;
      var s = JSON.parse(raw);
      if (!s.edges) return;
      var changed = false;
      Object.keys(s.edges).forEach(function (eid) {
        var e = s.edges[eid];
        if (e.source === deviceId || e.target === deviceId) {
          delete s.edges[eid];
          changed = true;
        }
      });
      if (changed) localStorage.setItem(DATA_KEY, JSON.stringify(s));
    } catch (e) {}
  }

  // Remove a stale device from the live graph + the shadow. cy.remove()
  // cascades to child ports + connected edges automatically; the shadow
  // edges are scrubbed separately so they don't come back on reload.
  function removeStaleDevice(id) {
    var node = cy.getElementById(id);
    if (node.length) node.remove();
    removeFromShadow("nodes", id);
    purgeShadowEdgesForDevice(id);
    var panel = document.getElementById("topology-detail");
    if (panel) panel.innerHTML = "<em>Click a node for details.</em>";
    saveCurrentPositions();
  }

  // Remove a stale cy edge + every shadow edge that maps to the same
  // canonical port-pair. The cy edge id is "pair:<endpointA>|<endpointB>"
  // where each endpoint is "<device>::<port>"; shadow edges store device
  // ids + sport/tport separately, so we have to translate.
  function removeStaleEdge(cyEdgeId) {
    var edge = cy.getElementById(cyEdgeId);
    if (edge.length) edge.remove();
    if (cyEdgeId.indexOf("pair:") !== 0) return;
    var pair = cyEdgeId.slice("pair:".length).split("|");
    if (pair.length !== 2) return;
    var a = pair[0].split("::"), b = pair[1].split("::");
    if (a.length !== 2 || b.length !== 2) return;
    var devA = a[0], portA = a[1], devB = b[0], portB = b[1];
    try {
      var raw = localStorage.getItem(DATA_KEY);
      if (!raw) return;
      var s = JSON.parse(raw);
      if (!s.edges) return;
      var changed = false;
      Object.keys(s.edges).forEach(function (eid) {
        var e = s.edges[eid];
        var match =
          (e.source === devA && e.target === devB && e.sport === portA && e.tport === portB) ||
          (e.source === devB && e.target === devA && e.sport === portB && e.tport === portA);
        if (match) { delete s.edges[eid]; changed = true; }
      });
      if (changed) localStorage.setItem(DATA_KEY, JSON.stringify(s));
    } catch (e) {}
    var panel = document.getElementById("topology-detail");
    if (panel) panel.innerHTML = "<em>Click a node for details.</em>";
  }

  // Reconstruct a /topology/json-shape object from the accumulated shadow.
  // The merge path then treats it just like a fresh server response —
  // every item gets a .stale class on initial render, fresh fetches
  // un-stale items they cover.
  function shadowAsData() {
    var s = readShadow();
    var nodes = Object.keys(s.nodes)
      .map(function (k) { return s.nodes[k]; })
      .filter(function (n) { return n && n.id; });
    var edges = Object.keys(s.edges)
      .map(function (k) { return s.edges[k]; })
      .filter(function (e) { return e && e.id && e.source && e.target; });
    if (!nodes.length && !edges.length) return null;
    return {
      nodes: nodes,
      edges: edges,
      node_count: nodes.length,
      edge_count: edges.length,
      fetched_at: null,
    };
  }

  // Apply saved positions to nodes that have them. Returns true if EVERY
  // node was placed from saved state (i.e. we can skip the auto-layout).
  function applySavedPositions(saved) {
    if (!saved) return false;
    var allPlaced = true;
    cy.nodes().forEach(function (n) {
      var p = saved[n.id()];
      if (p && typeof p.x === "number" && typeof p.y === "number") {
        n.position({ x: p.x, y: p.y });
      } else {
        allPlaced = false;
      }
    });
    return allPlaced;
  }

  function escapeHtml(s) {
    return String(s || "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function portChildId(deviceId, portName) {
    return deviceId + "::" + (portName || "?");
  }

  // Natural compare: "Gi1/0/2" < "Gi1/0/10" by treating runs of digits as
  // numbers. Falls back to ordinary string compare for letter chunks.
  function naturalCompare(a, b) {
    var ax = String(a || "").split(/(\d+)/).filter(function (s) { return s !== ""; });
    var bx = String(b || "").split(/(\d+)/).filter(function (s) { return s !== ""; });
    for (var i = 0; i < ax.length && i < bx.length; i++) {
      var ai = ax[i], bi = bx[i];
      var an = /^\d+$/.test(ai), bn = /^\d+$/.test(bi);
      if (an && bn) {
        var diff = parseInt(ai, 10) - parseInt(bi, 10);
        if (diff !== 0) return diff;
      } else if (ai !== bi) {
        return ai < bi ? -1 : 1;
      }
    }
    return ax.length - bx.length;
  }

  function nodeDeviceData(node) {
    // node may be a port (child) or device (parent); return the device data.
    if (node.isChild()) return node.parent().first().data();
    return node.data();
  }

  function renderDetail(node) {
    var d = nodeDeviceData(node);
    var deviceId = d.id;
    var deviceNode = cy.getElementById(deviceId);
    var isStale = deviceNode.length && deviceNode.hasClass("stale");
    var isAlarmed = deviceNode.length && deviceNode.hasClass("stale-unacked");
    var isImportant = loadImportant().has(deviceId);

    var parts = [];
    parts.push('<h3 style="margin:0 0 8px 0;">' + escapeHtml(d.label || d.id) + "</h3>");
    parts.push("<p>");
    if (d.managed) {
      parts.push('<span style="background:#e0e0e0; padding:1px 6px; border-radius:3px;">managed</span> ');
    }
    if (isImportant) {
      parts.push('<span style="background:#1a5276; color:white; padding:1px 6px; border-radius:3px;">important</span> ');
    }
    if (isAlarmed) {
      parts.push('<span style="background:#c0392b; color:white; padding:1px 6px; border-radius:3px;">OFFLINE — unacknowledged</span> ');
    } else if (isStale) {
      parts.push('<span style="background:#f5f5f5; color:#aaa; padding:1px 6px; border-radius:3px; border:1px solid #ddd;">stale</span> ');
    }
    if (isAlarmed) {
      parts.push('<button data-action="ack-stale-device" data-id="' + escapeHtml(deviceId) +
        '" style="margin-left:8px;">Acknowledge offline</button>');
    }
    if (isStale) {
      parts.push('<button data-action="remove-stale-device" data-id="' + escapeHtml(deviceId) +
        '" style="margin-left:8px;">Remove from history</button>');
    }
    // Mark/unmark as important. Managed devices already alarm on stale, so
    // the toggle is only useful for unmanaged ones.
    if (!d.managed) {
      if (isImportant) {
        parts.push('<button data-action="unmark-important" data-id="' + escapeHtml(deviceId) +
          '" style="margin-left:8px;">Unmark important</button>');
      } else {
        parts.push('<button data-action="mark-important" data-id="' + escapeHtml(deviceId) +
          '" style="margin-left:8px;" title="Alarm (red) if this device disappears from CDP">Mark as important</button>');
      }
    }
    parts.push("</p>");

    var fields = [
      ["Description", d.description],
      ["Role", d.role],
      ["IP", d.ip],
      ["Platform", d.platform],
      ["Version", d.version],
    ];
    parts.push("<table style='width:100%;'>");
    fields.forEach(function (kv) {
      if (kv[1]) {
        parts.push("<tr><th style='text-align:left; padding-right:6px; vertical-align:top;'>" +
          escapeHtml(kv[0]) + "</th><td>" + escapeHtml(kv[1]) + "</td></tr>");
      }
    });
    parts.push("</table>");
    if (d.href) {
      parts.push('<p style="margin-top:10px;"><a href="' + escapeHtml(d.href) + '">Open device page →</a></p>');
    }
    // List adjacencies for this device (collect from all its child ports).
    // Stale adjacencies get a gray "(stale)" badge + a tiny remove button.
    var edges = deviceNode.children().connectedEdges();
    if (edges.length) {
      parts.push("<h4 style='margin:12px 0 4px 0;'>Adjacencies (" + edges.length + ")</h4><ul style='padding-left:18px; margin:0;'>");
      edges.forEach(function (e) {
        var ed = e.data();
        var direction = ed._sourceDevice === deviceId ? "→" : "←";
        var localPort = ed._sourceDevice === deviceId ? ed.sport : ed.tport;
        var remotePort = ed._sourceDevice === deviceId ? ed.tport : ed.sport;
        var other = ed._sourceDevice === deviceId ? ed._targetDevice : ed._sourceDevice;
        var edgeStale = e.hasClass("stale");
        var liStyle = edgeStale ? ' style="color:#888;"' : "";
        var staleBadge = edgeStale
          ? ' <span style="color:#aaa; font-style:italic;">(stale)</span>' +
            ' <button data-action="remove-stale-edge" data-id="' + escapeHtml(ed.id) +
            '" title="Remove this adjacency from history" style="padding:0 6px;">×</button>'
          : "";
        parts.push("<li" + liStyle + "><code>" + escapeHtml(localPort || "?") + "</code> " +
          direction + " <strong>" + escapeHtml(other) + "</strong>" +
          " <code>" + escapeHtml(remotePort || "?") + "</code>" + staleBadge + "</li>");
      });
      parts.push("</ul>");
    }
    var panel = document.getElementById("topology-detail");
    panel.innerHTML = parts.join("");
    // Wire delete buttons via event delegation. Re-rendered every call,
    // so no need to track listeners across invocations.
    panel.querySelectorAll("[data-action]").forEach(function (btn) {
      btn.addEventListener("click", function () {
        var act = btn.getAttribute("data-action");
        var id = btn.getAttribute("data-id");
        if (act === "remove-stale-device") removeStaleDevice(id);
        else if (act === "remove-stale-edge") removeStaleEdge(id);
        else if (act === "ack-stale-device") ackDevice(id);
        else if (act === "mark-important") markImportant(id);
        else if (act === "unmark-important") unmarkImportant(id);
      });
    });
  }

  function buildElements(data, managedOnly) {
    var els = [];
    var visibleDevices = new Set();

    // 1. Decide which device nodes are visible.
    data.nodes.forEach(function (n) {
      if (managedOnly && !n.managed) return;
      visibleDevices.add(n.id);
    });

    // 2. Emit parent (device) nodes.
    data.nodes.forEach(function (n) {
      if (!visibleDevices.has(n.id)) return;
      els.push({
        group: "nodes",
        data: n,
        classes: "device " + (n.managed ? "managed" : "unmanaged"),
      });
    });

    // 3. Gather port usage per device (each port appears at most once even
    //    if multiple edges land on it). We emit children in natural-sorted
    //    order — fcose's compound layout uses insertion order as a hint,
    //    and the post-layout alignment pass strictly orders them after.
    var portsByDevice = {};
    data.edges.forEach(function (e) {
      if (!visibleDevices.has(e.source) || !visibleDevices.has(e.target)) return;
      (portsByDevice[e.source] = portsByDevice[e.source] || new Set()).add(e.sport || "?");
      (portsByDevice[e.target] = portsByDevice[e.target] || new Set()).add(e.tport || "?");
    });
    Object.keys(portsByDevice).forEach(function (deviceId) {
      var sorted = Array.from(portsByDevice[deviceId]).sort(naturalCompare);
      sorted.forEach(function (portName) {
        els.push({
          group: "nodes",
          data: { id: portChildId(deviceId, portName), parent: deviceId, label: portName },
          classes: "port",
        });
      });
    });

    // 4. Dedupe CDP adjacencies by the unordered port pair (CDP usually
    //    reports both directions for the same physical link). The merged
    //    edge gets a "bidirectional" class which the style turns into a
    //    second mid-arrow at the source side, so a single line carries
    //    two arrows offset along it — both pointing outward at their
    //    respective ports.
    var pairs = new Map();
    data.edges.forEach(function (e) {
      if (!visibleDevices.has(e.source) || !visibleDevices.has(e.target)) return;
      var src = portChildId(e.source, e.sport);
      var tgt = portChildId(e.target, e.tport);
      var key = src < tgt ? src + "|" + tgt : tgt + "|" + src;
      var existing = pairs.get(key);
      if (existing) {
        existing.bidirectional = true;
        return;
      }
      // Use the canonical pair key as the id, so the same physical link
      // keeps the same id across polls — important for the diff-merge
      // path on auto-refresh (server-side IDs depend on which direction
      // it saw first).
      pairs.set(key, {
        id: "pair:" + key,
        source: src,
        target: tgt,
        sport: e.sport,
        tport: e.tport,
        sourceDevice: e.source,
        targetDevice: e.target,
        bidirectional: false,
      });
    });
    pairs.forEach(function (p) {
      els.push({
        group: "edges",
        data: {
          id: p.id,
          source: p.source,
          target: p.target,
          sport: p.sport,
          tport: p.tport,
          _sourceDevice: p.sourceDevice,
          _targetDevice: p.targetDevice,
        },
        classes: p.bidirectional ? "bidirectional" : "",
      });
    });

    return els;
  }

  function styles() {
    return [
      // ── Device (compound parent) ─────────────────────────────────────
      {
        selector: "node.device",
        style: {
          shape: "round-rectangle",
          "background-color": "#ffffff",
          "background-opacity": 1,
          "border-color": "#888",
          "border-width": 1,
          // Label = device name + (when present) description on a second
          // line. text-wrap: wrap is required for the embedded "\n" to
          // render; text-max-width keeps long descriptions from blowing
          // out the node width.
          label: function (n) {
            var d = n.data();
            return d.description ? (d.label + "\n" + d.description) : d.label;
          },
          "text-wrap": "wrap",
          "text-max-width": "180px",
          "text-valign": "top",
          "text-halign": "center",
          "text-margin-y": -4,
          "font-size": "12px",
          "font-family": "Verdana, sans-serif",
          "font-weight": "bold",
          padding: "10px",
        },
      },
      {
        selector: "node.device.managed",
        style: {
          "background-color": "#e0e0e0",
          "border-color": "#444",
        },
      },
      {
        selector: "node.device:selected",
        style: {
          "border-color": "#2980b9",
          "border-width": 3,
        },
      },
      // ── Port (compound child) ────────────────────────────────────────
      {
        selector: "node.port",
        style: {
          shape: "rectangle",
          "background-color": "#ffffff",
          "border-color": "#aaa",
          "border-width": 1,
          label: "data(label)",
          "text-valign": "center",
          "text-halign": "center",
          "font-size": "9px",
          "font-family": "monospace",
          padding: "2px",
          width: "label",
          height: "label",
        },
      },
      // ── Edges ────────────────────────────────────────────────────────
      // Single line per port pair with open "vee" arrows in the middle.
      // Unidirectional: one mid-target-arrow toward target. Bidirectional:
      // also mid-source-arrow on the source side. Vees give the same
      // directional read as triangles but visually lighter — works for
      // single arrows AND for the `><` clash at the midpoint of mutual
      // adjacencies. Swap "vee" for chevron/circle/diamond/tee here if
      // you want a different style.
      {
        selector: "edge",
        style: {
          width: 1,
          "line-color": "#666",
          "curve-style": "bezier",
          "mid-target-arrow-shape": "vee",
          "mid-target-arrow-color": "#666",
          "arrow-scale": 1.4,
          "source-endpoint": "outside-to-line",
          "target-endpoint": "outside-to-line",
          "source-distance-from-node": 1,
          "target-distance-from-node": 1,
        },
      },
      {
        selector: "edge.bidirectional",
        style: {
          "mid-source-arrow-shape": "vee",
          "mid-source-arrow-color": "#666",
        },
      },
      {
        selector: "edge:selected",
        style: {
          "line-color": "#2980b9",
          "mid-target-arrow-color": "#2980b9",
          width: 2,
        },
      },
      // ── Selected / peer-port highlight ───────────────────────────────
      // Use the same blue treatment for both the actively-selected port
      // and any peers added by the .highlighted class on selection.
      {
        selector: "node.port:selected, node.port.highlighted",
        style: {
          "border-color": "#2980b9",
          "border-width": 2,
          "background-color": "#eaf3fb",
        },
      },
      // Highlight: thicker blue line + matching blue mid-arrows on both
      // sides (mid-source-arrow-color covers the bidirectional case).
      {
        selector: "edge.highlighted",
        style: {
          "line-color": "#2980b9",
          "mid-target-arrow-color": "#2980b9",
          "mid-source-arrow-color": "#2980b9",
          width: 3,
        },
      },
      // Search dimming: applied to anything that doesn't match the
      // current /find/ query. Keeps matched devices full-color so they
      // stand out against the faded rest of the graph.
      {
        selector: "node.dim",
        style: { opacity: 0.2 },
      },
      {
        selector: "edge.dim",
        style: { opacity: 0.12 },
      },
      // Stale = present in earlier polls, missing from the latest one.
      // Render very pale so they fade into the background without being
      // removed outright — useful for spotting devices that just dropped.
      {
        selector: "node.device.stale",
        style: {
          "background-color": "#f5f5f5",
          "background-opacity": 0.6,
          "border-color": "#cfcfcf",
          color: "#aaa",
        },
      },
      // Important = operator-flagged. Subtle double-border accent so the
      // device is recognizable even when healthy/online. The .stale and
      // .stale-unacked rules below override the border color/style for
      // stale and alarmed states respectively.
      {
        selector: "node.device.important",
        style: {
          "border-style": "double",
          "border-width": 4,
          "border-color": "#1a5276",
        },
      },
      // Managed-or-important devices that have gone stale and the operator
      // has NOT yet acknowledged the outage. Render alarming red so they
      // stand out against the gray sea of stale APs/cameras.
      {
        selector: "node.device.stale-unacked",
        style: {
          "background-color": "#ffe5e5",
          "background-opacity": 1,
          "border-color": "#c0392b",
          "border-style": "solid",
          "border-width": 2,
          color: "#c0392b",
        },
      },
      {
        selector: "node.port.stale",
        style: {
          "background-color": "#fafafa",
          "border-color": "#dcdcdc",
          color: "#bbb",
        },
      },
      {
        selector: "edge.stale",
        style: {
          "line-color": "#dcdcdc",
          "mid-target-arrow-color": "#dcdcdc",
          "mid-source-arrow-color": "#dcdcdc",
          color: "#bbb",
        },
      },
    ];
  }

  function layoutOptions(name) {
    var common = { name: name, animate: false, fit: true, padding: 40 };
    if (name === "fcose") {
      return Object.assign(common, {
        // "proof" runs more passes and yields fewer crossings; ~2× slower
        // than "default" but the difference is invisible on graphs <1k nodes.
        quality: "proof",
        randomize: true,
        nodeSeparation: 90,
        idealEdgeLength: 140,
        nodeRepulsion: 9000,
        gravity: 0.18,
        gravityCompound: 1.6,
        numIter: 4000,
        // Tile child nodes inside compound parents, with tight spacing —
        // the post-layout pass re-aligns them strictly anyway.
        tile: true,
        tilingPaddingHorizontal: 4,
        tilingPaddingVertical: 4,
        packComponents: true,
      });
    }
    if (name === "cose") {
      return Object.assign(common, {
        idealEdgeLength: 220,
        nodeRepulsion: 12000,
        edgeElasticity: 50,
        nestingFactor: 1.2,
        gravity: 0.15,
        numIter: 2500,
        initialTemp: 250,
        randomize: false,
      });
    }
    if (name === "concentric") {
      return Object.assign(common, {
        concentric: function (node) { return node.degree(); },
        levelWidth: function () { return 1; },
        minNodeSpacing: 30,
      });
    }
    if (name === "grid") return Object.assign(common, { spacingFactor: 1.6 });
    if (name === "circle") return Object.assign(common, { spacingFactor: 1.6 });
    if (name === "breadthfirst") {
      return Object.assign(common, {
        spacingFactor: 1.4,
        directed: true,
      });
    }
    return common;
  }

  // ─────────────────────────────────────────────────────────────────────
  //  Custom "ranked" layout (BFS-from-leaves + radial placement).
  //
  //  Rank assignment:
  //    - Each device starts unranked. Leaves (degree ≤ 1) get rank 1.
  //    - Each subsequent pass finds unranked devices adjacent to any
  //      already-ranked device, assigning them rank = max_rank + 1 + degree.
  //    - Disconnected components / unbreakable cycles get the next free
  //      rank in arbitrary order.
  //
  //  Placement:
  //    - Sort devices by rank descending (most-central hub first).
  //    - Place rank-1 (= highest rank) device at origin.
  //    - Each subsequent device picks its highest-ranked already-placed
  //      neighbor as a "parent", then places itself at the angle around
  //      that parent that is most distant from any sibling already placed
  //      around the same parent. Distance from parent is fixed (spacing).
  //
  //  Port-position seeding is a TODO — currently angles ignore which port
  //  on the parent the edge lands on.

  function computeRanks(devices) {
    // Build device-level adjacency (unique neighbors only) from the
    // port-to-port edges.
    var neighbors = {};
    devices.forEach(function (d) { neighbors[d.id()] = new Set(); });
    cy.edges().forEach(function (e) {
      var s = e.source().isChild() ? e.source().parent().first().id() : e.source().id();
      var t = e.target().isChild() ? e.target().parent().first().id() : e.target().id();
      if (s !== t && neighbors[s] && neighbors[t]) {
        neighbors[s].add(t);
        neighbors[t].add(s);
      }
    });

    var ranks = {};
    var ranked = new Set();
    var maxRank = 0;

    // Pass 1: leaves
    devices.forEach(function (d) {
      var id = d.id();
      if (neighbors[id].size <= 1) {
        ranks[id] = 1;
        ranked.add(id);
      }
    });
    maxRank = ranked.size > 0 ? 1 : 0;

    // No leaves at all (everything in cycles) — seed with the lowest-degree node.
    if (ranked.size === 0 && devices.length > 0) {
      var seed = devices[0].id();
      var minDeg = Infinity;
      devices.forEach(function (d) {
        var deg = neighbors[d.id()].size;
        if (deg < minDeg) { minDeg = deg; seed = d.id(); }
      });
      ranks[seed] = 1;
      ranked.add(seed);
      maxRank = 1;
    }

    // Subsequent passes
    while (ranked.size < devices.length) {
      var thisRound = [];
      devices.forEach(function (d) {
        var id = d.id();
        if (ranked.has(id)) return;
        var connects = false;
        neighbors[id].forEach(function (n) { if (ranked.has(n)) connects = true; });
        if (connects) thisRound.push(id);
      });
      if (thisRound.length === 0) {
        // Disconnected component — pick lowest-degree unranked.
        var pick = null, minDegU = Infinity;
        devices.forEach(function (d) {
          if (ranked.has(d.id())) return;
          var deg = neighbors[d.id()].size;
          if (deg < minDegU) { minDegU = deg; pick = d.id(); }
        });
        if (pick) {
          ranks[pick] = maxRank + 1;
          ranked.add(pick);
          maxRank += 1;
        } else {
          break;
        }
        continue;
      }
      thisRound.forEach(function (id) {
        ranks[id] = maxRank + 1 + neighbors[id].size;
        ranked.add(id);
      });
      Object.keys(ranks).forEach(function (k) {
        if (ranks[k] > maxRank) maxRank = ranks[k];
      });
    }

    return { ranks: ranks, neighbors: neighbors };
  }

  // Pick the angle (in radians, range -π..π) that is most distant from
  // any angle already taken around the same parent.
  function pickFreeAngle(taken, segments) {
    if (!taken || taken.length === 0) return 0;
    var best = 0, bestDist = -1;
    for (var i = 0; i < segments; i++) {
      var a = (i / segments) * 2 * Math.PI - Math.PI;
      var minDist = Infinity;
      taken.forEach(function (t) {
        var d = Math.abs(a - t);
        if (d > Math.PI) d = 2 * Math.PI - d;
        if (d < minDist) minDist = d;
      });
      if (minDist > bestDist) { bestDist = minDist; best = a; }
    }
    return best;
  }

  // Return the port-child on `parentId` that connects to `childId`, or null.
  // Used for port-position-aware ordering during placement.
  function portOnDeviceFor(parentId, childId) {
    var parentNode = cy.getElementById(parentId);
    if (!parentNode.length) return null;
    var edges = parentNode.children().connectedEdges();
    for (var i = 0; i < edges.length; i++) {
      var e = edges[i];
      var sParent = e.source().isChild() ? e.source().parent().first().id() : e.source().id();
      var tParent = e.target().isChild() ? e.target().parent().first().id() : e.target().id();
      if (sParent === parentId && tParent === childId) return e.source();
      if (tParent === parentId && sParent === childId) return e.target();
    }
    return null;
  }

  function rankedLayout() {
    var devices = cy.nodes(".device").toArray();
    if (!devices.length) return;
    var r = computeRanks(devices);
    var ranks = r.ranks;
    var neighbors = r.neighbors;

    // 1) Process devices in rank-descending order; for each, the parent in
    //    the placement tree is its highest-ranked already-processed neighbor.
    var sorted = devices.slice().sort(function (a, b) {
      return (ranks[b.id()] || 0) - (ranks[a.id()] || 0);
    });
    var parentOf = {};
    var childrenOf = {};
    var processed = new Set();
    var disconnectedRoots = [];
    sorted.forEach(function (d, idx) {
      var id = d.id();
      if (idx === 0) { processed.add(id); return; }
      var processedN = [];
      neighbors[id].forEach(function (n) { if (processed.has(n)) processedN.push(n); });
      if (processedN.length === 0) {
        // Separate component — this node becomes a new sub-root.
        disconnectedRoots.push(id);
        processed.add(id);
        return;
      }
      processedN.sort(function (a, b) { return (ranks[b] || 0) - (ranks[a] || 0); });
      var p = processedN[0];
      parentOf[id] = p;
      if (!childrenOf[p]) childrenOf[p] = [];
      childrenOf[p].push(id);
      processed.add(id);
    });

    // 2) Compute subtree size (descendant count + 1) for each device.
    var subtreeSize = {};
    function sizeOf(id) {
      if (subtreeSize[id] != null) return subtreeSize[id];
      var s = 1;
      (childrenOf[id] || []).forEach(function (c) { s += sizeOf(c); });
      subtreeSize[id] = s;
      return s;
    }
    sorted.forEach(function (d) { sizeOf(d.id()); });

    // 3) Radial tree placement. Each subtree gets an angular sector
    //    proportional to its size so hubs with many descendants spread
    //    wider. Children are ordered by the position of the port on the
    //    parent that connects to them — port at top of parent's column
    //    → child placed at the most "upward" angle in the sector. This
    //    keeps the visual edges from crossing inside their hub's wedge.

    var placed = {};
    var BASE_RADIUS = 280;     // ring 0 → ring 1 distance
    var RING_STEP   = 200;     // additional distance per ring

    function place(id, x, y, parentAngle, sectorWidth, depth) {
      placed[id] = { x: x, y: y };
      var kids = (childrenOf[id] || []).slice();
      if (!kids.length) return;

      // Sort kids by the y-position of their port on this device (parent).
      // Falls back to subtree size order for ones without a known port.
      kids.sort(function (a, b) {
        var pa = portOnDeviceFor(id, a);
        var pb = portOnDeviceFor(id, b);
        if (pa && pb) return pa.position().y - pb.position().y;
        if (pa) return -1;
        if (pb) return 1;
        return 0;
      });

      // Sector center: opposite the parent (for non-root). Root uses full 2π.
      var sectorCenter = depth === 0 ? -Math.PI / 2 : (parentAngle + Math.PI);
      var maxWidth = depth === 0 ? 2 * Math.PI : Math.min(sectorWidth, (5 / 6) * Math.PI);
      var sectorStart = sectorCenter - maxWidth / 2;

      var totalKids = 0;
      kids.forEach(function (c) { totalKids += subtreeSize[c]; });

      var anglePos = sectorStart;
      var radius = BASE_RADIUS + depth * RING_STEP;
      kids.forEach(function (c) {
        var slice = maxWidth * (subtreeSize[c] / totalKids);
        var childAngle = anglePos + slice / 2;
        var cx = x + Math.cos(childAngle) * radius;
        var cy = y + Math.sin(childAngle) * radius;
        // The angle from the child back to its parent is childAngle + π.
        place(c, cx, cy, childAngle + Math.PI, slice, depth + 1);
        anglePos += slice;
      });
    }

    var root = sorted[0].id();
    place(root, 0, 0, 0, 2 * Math.PI, 0);

    // Disconnected components: place each off to the side, far from root.
    var offset = 2500;
    disconnectedRoots.forEach(function (id, i) {
      var ang = (i / Math.max(1, disconnectedRoots.length)) * 2 * Math.PI;
      place(id, Math.cos(ang) * offset, Math.sin(ang) * offset, 0, 2 * Math.PI, 0);
    });

    // Any device that somehow wasn't reached (shouldn't happen, but
    // defensive): scatter at distance.
    devices.forEach(function (d, i) {
      if (placed[d.id()]) return;
      var ang = i * 0.7;
      placed[d.id()] = { x: Math.cos(ang) * offset * 2, y: Math.sin(ang) * offset * 2 };
    });

    cy.startBatch();
    Object.keys(placed).forEach(function (id) {
      var n = cy.getElementById(id);
      if (n.length) n.position(placed[id]);
    });
    cy.endBatch();
  }

  // After the macro layout runs, place each device's port-children in a
  // vertical column centered on the device, sorted by port name. The parent
  // auto-resizes to fit; cy.fit() afterward refreshes the viewport.
  function alignChildrenInColumns() {
    if (!cy) return;
    cy.startBatch();
    cy.nodes(".device").forEach(function (parent) {
      var children = parent.children().sort(function (a, b) {
        return naturalCompare(a.data("label"), b.data("label"));
      });
      if (!children.length) return;
      var pos = parent.position();
      var spacing = 16; // px between port rows
      var startY = pos.y - ((children.length - 1) * spacing) / 2;
      children.forEach(function (child, idx) {
        child.position({ x: pos.x, y: startY + idx * spacing });
      });
    });
    cy.endBatch();
  }

  function fitAndLayout() {
    if (!cy) return;
    var name = document.getElementById("topology-layout").value;
    if (name === "ranked") {
      rankedLayout();
      alignChildrenInColumns();
    } else {
      cy.layout(layoutOptions(name)).run();
      if (name === "fcose" || name === "cose") {
        alignChildrenInColumns();
      }
    }
    cy.fit(undefined, 30);
  }

  // Return the set of unique device-id neighbors of `deviceId` (derived
  // from its port-children's connected edges).
  function deviceNeighborsOf(deviceId) {
    var node = cy.getElementById(deviceId);
    var neighbors = new Set();
    if (!node.length) return neighbors;
    node.children().connectedEdges().forEach(function (e) {
      var sParent = e.source().isChild() ? e.source().parent().first().id() : e.source().id();
      var tParent = e.target().isChild() ? e.target().parent().first().id() : e.target().id();
      var other = sParent === deviceId ? tParent : sParent;
      if (other !== deviceId) neighbors.add(other);
    });
    return neighbors;
  }

  // Walk the device graph breadth-first starting from `rootId`, returning
  // the set of device ids reachable in <= `depth` hops (inclusive of root).
  function bfsDevices(rootId, depth) {
    var visited = new Set([rootId]);
    var frontier = [rootId];
    for (var d = 0; d < depth; d++) {
      var next = [];
      frontier.forEach(function (id) {
        deviceNeighborsOf(id).forEach(function (other) {
          if (!visited.has(other)) {
            visited.add(other);
            next.push(other);
          }
        });
      });
      frontier = next;
      if (!frontier.length) break;
    }
    return visited;
  }

  // For every unselected device that's a "leaf" (degree 1) whose single
  // neighbor is in the selection, pull it in too. Lets the user grab a
  // hub and have its dangling endpoints (APs, cameras, codecs with one
  // CDP adjacency) come along for the ride — without having to bump the
  // BFS depth and accidentally swallow whole other hubs.
  function augmentSelectionWithLeaves(selected) {
    cy.nodes(".device").forEach(function (d) {
      var id = d.id();
      if (selected.has(id)) return;
      var ns = deviceNeighborsOf(id);
      if (ns.size !== 1) return;
      var only = ns.values().next().value;
      if (selected.has(only)) selected.add(id);
    });
  }

  // ctrl+right-click on a device cycles a BFS-radius selection:
  //   1st click  → select clicked device + its 1-hop neighbors
  //   2nd click  → extend to 2-hop
  //   ...
  // Clicking a different device resets back to depth 1. Cytoscape's
  // built-in multi-select drag then moves the whole set together when
  // the user grabs any one of them.
  function handleCtrlRightClick(evt) {
    var orig = evt.originalEvent;
    if (!orig || !(orig.ctrlKey || orig.metaKey)) return;
    orig.preventDefault();

    var device = evt.target;
    if (!device.hasClass("device")) return;
    var id = device.id();

    if (!bfsState || bfsState.rootId !== id) {
      bfsState = { rootId: id, depth: 1 };
    } else {
      bfsState.depth += 1;
    }

    var set = bfsDevices(id, bfsState.depth);
    // Pull in any "single-connected" devices hanging off the BFS frontier
    // (APs/cameras/codecs with a single CDP adjacency to one of the
    // already-selected devices). They naturally belong to the moving
    // cluster without requiring another depth step.
    augmentSelectionWithLeaves(set);

    suppressSelectHandler = true;
    cy.elements().unselect();
    set.forEach(function (devId) {
      var n = cy.getElementById(devId);
      if (n.length) n.select();
    });
    suppressSelectHandler = false;

    // Update the status line so the user sees the current radius + count.
    var status = document.getElementById("topology-status");
    if (status) {
      status.textContent =
        "Selected " + set.size + " devices (root=" + id +
        ", depth=" + bfsState.depth + ")";
    }
  }

  // Apply the search-box filter: dim any device (+ its ports + its edges)
  // whose id/label/description doesn't contain the query (case-insensitive,
  // case-sensitive if the query has any uppercase). Edges stay full-color
  // when at least one of their endpoints matches.
  function applySearch() {
    if (!cy) return;
    var inputEl = document.getElementById("topology-search");
    if (!inputEl) return;
    var raw = inputEl.value;
    var q = raw.trim();
    cy.elements().removeClass("dim");
    renderSearchResults(q, []);
    if (q === "") return;
    var caseSensitive = q.toLowerCase() !== q;
    var needle = caseSensitive ? q : q.toLowerCase();

    var matched = new Set();
    var matches = [];
    cy.nodes(".device").forEach(function (d) {
      var data = d.data();
      var hay = [data.id, data.label, data.description, data.role, data.ip, data.platform]
        .filter(function (s) { return s; })
        .join("  ");
      if (!caseSensitive) hay = hay.toLowerCase();
      if (hay.indexOf(needle) !== -1) {
        matched.add(data.id);
        matches.push(data);
      }
    });

    cy.nodes(".device").forEach(function (d) {
      if (matched.has(d.id())) return;
      d.addClass("dim");
      d.children().addClass("dim");
    });
    cy.edges().forEach(function (e) {
      var sParent = e.source().isChild() ? e.source().parent().first().id() : e.source().id();
      var tParent = e.target().isChild() ? e.target().parent().first().id() : e.target().id();
      if (!matched.has(sParent) && !matched.has(tParent)) e.addClass("dim");
    });

    matches.sort(function (a, b) {
      return (a.label || a.id).localeCompare(b.label || b.id);
    });
    renderSearchResults(q, matches);
  }

  function renderSearchResults(query, matches) {
    var list = document.getElementById("topology-search-results");
    if (!list) return;
    if (query === "") {
      list.innerHTML = "";
      list.hidden = true;
      return;
    }
    var html = [];
    if (matches.length === 0) {
      html.push('<li class="sr-empty">No matches</li>');
    } else {
      matches.slice(0, 50).forEach(function (d) {
        var name = d.label || d.id;
        var metaBits = [];
        if (d.description) metaBits.push(d.description);
        if (d.ip) metaBits.push(d.ip);
        var meta = metaBits.join(" · ");
        html.push(
          '<li role="option" data-id="' + escapeHtml(d.id) + '">' +
          '<div class="sr-name">' + escapeHtml(name) + '</div>' +
          (meta ? '<div class="sr-meta">' + escapeHtml(meta) + '</div>' : '') +
          '</li>'
        );
      });
      if (matches.length > 50) {
        html.push('<li class="sr-empty">… and ' + (matches.length - 50) + ' more</li>');
      }
    }
    list.innerHTML = html.join("");
    list.hidden = false;
    list.querySelectorAll("li[data-id]").forEach(function (li) {
      li.addEventListener("click", function () { flyToDevice(li.getAttribute("data-id")); });
    });
  }

  // Quick side-to-side pan oscillation — a "no-no" headshake. Used when
  // the user re-selects the device that's already in focus.
  function shakeViewport() {
    if (!cy) return;
    var origin = { x: cy.pan().x, y: cy.pan().y };
    var amp = 18;
    var step = 70;
    var steps = [
      { x: origin.x - amp,     y: origin.y },
      { x: origin.x + amp,     y: origin.y },
      { x: origin.x - amp * 0.6, y: origin.y },
      { x: origin.x + amp * 0.6, y: origin.y },
      { x: origin.x,           y: origin.y },
    ];
    var i = 0;
    function next() {
      if (i >= steps.length) return;
      var to = steps[i++];
      cy.animate({ pan: to }, { duration: step, easing: "ease-in-out", complete: next });
    }
    next();
  }

  // Animated "fly-out then fly-in" zoom: first frame both the current view
  // and the target so the eye can track the trip, hold briefly at the apex,
  // then dive in on the target. If the target is already on screen, skip
  // the fly-out and just zoom in.
  function flyToDevice(id) {
    if (!cy) return;
    var node = cy.getElementById(id);
    if (!node || !node.length) return;
    // "No-no" shake if the user re-clicks the device that's already focused.
    var alreadyFocused = node.selected();
    cy.elements(":selected").unselect();
    node.select();
    renderDetail(node);
    if (alreadyFocused) {
      shakeViewport();
      return;
    }

    var targetPos = node.position();
    var nodeBb = node.boundingBox();
    var ext = cy.extent();
    var currentZoom = cy.zoom();
    var inZoom = Math.max(1.0, currentZoom);

    var panToTarget = function (z) {
      return { x: cy.width() / 2 - targetPos.x * z,
               y: cy.height() / 2 - targetPos.y * z };
    };

    // If the target is already comfortably on screen, skip the fly-out
    // and just zoom in — the round trip would feel jarring for nearby hops.
    var onScreen = nodeBb.x1 >= ext.x1 && nodeBb.x2 <= ext.x2 &&
                   nodeBb.y1 >= ext.y1 && nodeBb.y2 <= ext.y2;
    if (onScreen) {
      cy.animate({ zoom: inZoom, pan: panToTarget(inZoom) },
                 { duration: 400, easing: "ease-in-out" });
      return;
    }

    // Frame the union of (current viewport, target node) so the eye can
    // see both the starting view and the destination in the same shot.
    var bbX1 = Math.min(ext.x1, nodeBb.x1);
    var bbY1 = Math.min(ext.y1, nodeBb.y1);
    var bbX2 = Math.max(ext.x2, nodeBb.x2);
    var bbY2 = Math.max(ext.y2, nodeBb.y2);
    var bbW = Math.max(1, bbX2 - bbX1);
    var bbH = Math.max(1, bbY2 - bbY1);
    var bbCx = (bbX1 + bbX2) / 2;
    var bbCy = (bbY1 + bbY2) / 2;
    var pad = 60;
    var fitZoom = Math.min(
      (cy.width() - 2 * pad) / bbW,
      (cy.height() - 2 * pad) / bbH
    );
    // Never zoom *in* during the fly-out phase — the apex should always be
    // wider than the start. Clamp to a sane floor too.
    fitZoom = Math.max(0.1, Math.min(fitZoom, currentZoom * 0.95));

    cy.animate(
      { zoom: fitZoom,
        pan: { x: cy.width() / 2 - bbCx * fitZoom,
               y: cy.height() / 2 - bbCy * fitZoom } },
      {
        duration: 450,
        easing: "ease-out",
        complete: function () {
          // Hang at the apex for a beat so the eye locks onto the target,
          // then dive in.
          setTimeout(function () {
            cy.animate({ zoom: inZoom, pan: panToTarget(inZoom) },
                       { duration: 500, easing: "ease-in-out" });
          }, 280);
        },
      }
    );
  }

  function clearSearch() {
    var inputEl = document.getElementById("topology-search");
    if (inputEl) inputEl.value = "";
    applySearch();
    if (inputEl) inputEl.focus();
  }

  // Highlight the relevant siblings of `el`:
  //   port  → its connected edges + the peer port(s) at the other end
  //   edge  → the source + target ports + the edge itself
  // Previous highlights are cleared first.
  function highlightSiblings(el) {
    cy.elements(".highlighted").removeClass("highlighted");
    if (el.isEdge()) {
      el.addClass("highlighted");
      el.source().addClass("highlighted");
      el.target().addClass("highlighted");
      return;
    }
    if (el.isNode() && el.hasClass("port")) {
      el.connectedEdges().forEach(function (edge) {
        var peer = edge.source().id() === el.id() ? edge.target() : edge.source();
        peer.addClass("highlighted");
        edge.addClass("highlighted");
      });
    }
  }

  function render() {
    if (!lastData) return;
    var managedOnly = document.getElementById("topology-managed-only").checked;
    var status = document.getElementById("topology-status");
    var els = buildElements(lastData, managedOnly);
    if (cy) cy.destroy();
    cy = cytoscape({
      container: document.getElementById("cy"),
      elements: els,
      style: styles(),
      wheelSensitivity: 0.2,
    });
    // Detail panel update + sibling highlight on selection. Fires on
    // click (and on drag-start for nodes), so the highlight is visible
    // while moving a port. Edge selection lights up its two endpoint
    // ports as well — reciprocal of the port-select case.
    // suppressSelectHandler is flipped on during programmatic multi-
    // select (ctrl+right-click BFS) so the panel doesn't redraw per node.
    cy.on("select", "node", function (evt) {
      if (suppressSelectHandler) return;
      renderDetail(evt.target);
      highlightSiblings(evt.target);
    });
    cy.on("select", "edge", function (evt) {
      if (suppressSelectHandler) return;
      highlightSiblings(evt.target);
    });
    cy.on("unselect", function () {
      cy.elements(".highlighted").removeClass("highlighted");
    });
    // ctrl+right-click on a device cycles a BFS-radius selection.
    cy.on("cxttap", "node", handleCtrlRightClick);
    // Double-click on a device (or one of its ports) flies in to that
    // device, same animation as picking it from the search dropdown.
    cy.on("dbltap", "node", function (evt) {
      var n = evt.target;
      var deviceId = n.hasClass("device") ? n.id() :
                     (n.isChild() ? n.parent().first().id() : null);
      if (deviceId) flyToDevice(deviceId);
    });
    // Persist positions after the user moves any node.
    cy.on("dragfree", "node", saveCurrentPositions);

    // If we have saved positions for every visible node, use them and
    // skip the auto-layout (preserves the user's manual arrangement
    // across reloads). Any node missing from the saved set falls back
    // to a fresh layout pass for the whole graph.
    var saved = loadSavedPositions();
    if (saved && applySavedPositions(saved)) {
      cy.fit(undefined, 30);
    } else {
      fitAndLayout();
      // Seed the storage with the freshly-computed layout so subsequent
      // reloads pick it up before any user drag.
      saveCurrentPositions();
    }
    var when = lastData.fetched_at
      ? new Date(lastData.fetched_at).toLocaleTimeString()
      : "never";
    var visibleDevices = cy.nodes(".device").length;
    var visibleEdges = cy.edges().length;
    status.textContent =
      visibleDevices + "/" + lastData.node_count + " devices · " +
      visibleEdges + "/" + lastData.edge_count + " adjacencies · CDP sweep: " + when;
    // Reapply the search dim in case new elements arrived.
    applySearch();
  }

  function load() {
    var status = document.getElementById("topology-status");
    status.textContent = "loading…";
    fetch("/topology/json")
      .then(function (r) { return r.json(); })
      .then(function (data) {
        updateShadow(data);
        if (cy && lastData) {
          // We pre-rendered from the cache and marked things stale; now
          // merge the fresh data so seen items lose their stale class.
          mergeUpdate(data);
        } else {
          lastData = data;
          render();
        }
      })
      .catch(function (e) {
        status.textContent = "load failed: " + e;
        console.error("topology load failed:", e);
      });
  }

  // Position a freshly-added device near the centroid of its connected
  // neighbors so it's at least in the neighborhood, not stuck at (0,0).
  function positionNewDevice(node) {
    var positions = [];
    deviceNeighborsOf(node.id()).forEach(function (nid) {
      var n = cy.getElementById(nid);
      if (n.length && n.position()) positions.push(n.position());
    });
    if (!positions.length) {
      node.position({ x: 0, y: 0 });
      return;
    }
    var avgX = 0, avgY = 0;
    positions.forEach(function (p) { avgX += p.x; avgY += p.y; });
    avgX /= positions.length;
    avgY /= positions.length;
    // small jitter so multiple new neighbors of the same hub don't stack
    node.position({
      x: avgX + (Math.random() - 0.5) * 100,
      y: avgY + (Math.random() - 0.5) * 100,
    });
  }

  // Incrementally merge fresh /topology/json into the existing cy graph
  // without disturbing existing node positions. Adds new nodes/edges,
  // removes ones that vanished, updates data on existing ones.
  function mergeUpdate(newData) {
    if (!cy) {
      lastData = newData;
      render();
      return;
    }
    var managedOnly = document.getElementById("topology-managed-only").checked;
    var newEls = buildElements(newData, managedOnly);

    // Snapshot positions of EVERY existing node (devices + port-children)
    // before the merge. After the merge runs and alignChildrenInColumns
    // re-lays out all children into sorted columns, we restore each
    // pre-existing node's exact position. This preserves any user-driven
    // drags on port-children too — without snapshotting them, the column
    // realignment would silently snap them back to the default layout.
    // New nodes (devices or ports) added this round get positioned by
    // positionNewDevice / alignChildrenInColumns.
    var prevPositions = {};
    cy.nodes().forEach(function (n) {
      var p = n.position();
      prevPositions[n.id()] = { x: p.x, y: p.y };
    });

    // Build maps of new elements by id
    var newById = {};
    newEls.forEach(function (el) { newById[el.data.id] = el; });

    // 1) Mark elements no longer present as "stale" instead of removing
    //    them. Lets the operator still see (and click on) devices that
    //    have dropped out of CDP — they fade rather than vanish.
    cy.elements().forEach(function (el) {
      if (!newById[el.id()]) {
        el.addClass("stale");
      }
    });

    // 2) Add new elements; update existing ones in place.
    //    Add devices first (parents), then port-children, then edges.
    //    Within "add new", track newly-added devices so we can position
    //    them after all elements are present (so port-children → edges
    //    don't refer to unknown ids).
    var addedDeviceIds = [];

    var sorted = newEls.slice().sort(function (a, b) {
      // devices before ports before edges
      var priority = function (el) {
        if (el.group === "edges") return 2;
        if (el.classes && el.classes.indexOf("port") >= 0) return 1;
        return 0;
      };
      return priority(a) - priority(b);
    });

    sorted.forEach(function (el) {
      var existing = cy.getElementById(el.data.id);
      if (existing.length === 0) {
        // For new compound children, place at parent's current position so
        // the parent's centroid (= avg of children) doesn't get dragged
        // toward (0,0) by an unpositioned new child. alignChildrenInColumns
        // re-spreads them properly after the add pass.
        var addArgs = el;
        if (el.group === "nodes" && el.data.parent) {
          var parentNode = cy.getElementById(el.data.parent);
          if (parentNode.length) {
            var pp = parentNode.position();
            addArgs = Object.assign({}, el, { position: { x: pp.x, y: pp.y } });
          }
        }
        cy.add(addArgs);
        if (el.group === "nodes" && el.classes && el.classes.indexOf("device") >= 0) {
          addedDeviceIds.push(el.data.id);
        }
      } else {
        // Update data fields (label, description, ip, etc.) without
        // touching the position.
        existing.data(el.data);
        // Refresh classes (including dropping any prior .stale marker).
        if (el.classes) {
          var oldClasses = (existing.classes() || []).join(" ");
          if (oldClasses !== el.classes) {
            existing.classes(el.classes);
          }
        }
        // Coming back from stale → also clear any prior acknowledgement
        // so the next disconnect re-alarms.
        if (existing.hasClass("stale")) {
          unackDevice(existing.id());
        }
        existing.removeClass("stale");
        existing.removeClass("stale-unacked");
      }
    });

    // Position any newly-added devices near their connected neighbors,
    // unless a saved position exists for them.
    var saved = loadSavedPositions();
    addedDeviceIds.forEach(function (id) {
      var node = cy.getElementById(id);
      if (!node.length) return;
      var sp = saved && saved[id];
      if (sp) {
        node.position(sp);
      } else {
        positionNewDevice(node);
      }
    });

    // Realign port-children for any device whose port set changed.
    // This positions BOTH pre-existing and new ports into the sorted
    // column; the restore loop below puts pre-existing ones back to
    // exactly where they were, so only the new ports keep the column
    // placement.
    alignChildrenInColumns();

    // Restore positions of every pre-existing node (device + port).
    // Without this, alignChildrenInColumns would silently snap any
    // user-dragged port back to the default sorted column.
    Object.keys(prevPositions).forEach(function (id) {
      var node = cy.getElementById(id);
      if (node.length) node.position(prevPositions[id]);
    });

    // Persist the updated positions (new ones are now seeded).
    saveCurrentPositions();

    lastData = newData;
    var status = document.getElementById("topology-status");
    if (status) {
      var when = newData.fetched_at
        ? new Date(newData.fetched_at).toLocaleTimeString()
        : "never";
      var vd = cy.nodes(".device").length;
      var ve = cy.edges().length;
      status.textContent =
        vd + "/" + newData.node_count + " devices · " +
        ve + "/" + newData.edge_count + " adjacencies · CDP sweep: " + when;
    }
    // Reapply the search dim in case new elements arrived.
    applySearch();
    // Update which stale devices should be alarming (managed + not acked).
    refreshStaleAlarms();
  }

  function autoLoad() {
    fetch("/topology/json")
      .then(function (r) { return r.json(); })
      .then(function (data) { updateShadow(data); mergeUpdate(data); })
      .catch(function (e) { console.warn("topology auto-refresh failed:", e); });
  }

  var AUTO_REFRESH_MS = 30000;
  var autoRefreshTimer = null;
  function startAutoRefresh() {
    if (autoRefreshTimer) return;
    autoRefreshTimer = setInterval(autoLoad, AUTO_REFRESH_MS);
  }
  function stopAutoRefresh() {
    if (autoRefreshTimer) {
      clearInterval(autoRefreshTimer);
      autoRefreshTimer = null;
    }
  }

  function resetLayout() {
    if (!confirm("Clear saved layout and re-run the auto-layout?")) return;
    clearSavedPositions();
    render();
  }

  function clearHistory() {
    if (!confirm("Clear all cached topology history? This wipes every gray (stale) device and edge from the local cache.")) return;
    clearShadow();
    try { localStorage.removeItem(ACK_KEY); } catch (e) {}
    try { localStorage.removeItem(IMPORTANT_KEY); } catch (e) {}
    if (cy) {
      cy.elements(".stale").remove();
      cy.nodes(".device").removeClass("important stale-unacked");
      saveCurrentPositions();
    }
    var panel = document.getElementById("topology-detail");
    if (panel) panel.innerHTML = "<em>Click a node for details.</em>";
  }

  window.initTopology = function () {
    document.getElementById("topology-refresh").addEventListener("click", load);
    document.getElementById("topology-reset-layout").addEventListener("click", resetLayout);
    document.getElementById("topology-clear-history").addEventListener("click", clearHistory);
    document.getElementById("topology-managed-only").addEventListener("change", render);
    // Changing the layout choice is an explicit "re-run" — drop saved
    // positions so the new layout actually applies.
    document.getElementById("topology-layout").addEventListener("change", function () {
      clearSavedPositions();
      render();
    });
    // Auto-refresh: poll /topology/json every 30s and diff-merge into
    // the existing graph, preserving manual positions.
    var autoToggle = document.getElementById("topology-autorefresh");
    autoToggle.addEventListener("change", function () {
      if (autoToggle.checked) startAutoRefresh();
      else stopAutoRefresh();
    });
    document.getElementById("topology-search").addEventListener("input", applySearch);
    document.getElementById("topology-search-clear").addEventListener("click", clearSearch);
    // Hide the results dropdown when clicking anywhere outside the search.
    document.addEventListener("click", function (ev) {
      var wrap = ev.target.closest(".topology-search-wrap");
      if (!wrap) {
        var list = document.getElementById("topology-search-results");
        if (list) list.hidden = true;
      }
    });
    // Re-show the dropdown when the input regains focus (if there's a query).
    document.getElementById("topology-search").addEventListener("focus", function () {
      var inputEl = document.getElementById("topology-search");
      var list = document.getElementById("topology-search-results");
      if (inputEl && list && inputEl.value.trim() && list.innerHTML) list.hidden = false;
    });

    // Render whatever the shadow contains first, with everything stale;
    // the first /topology/json fetch will then un-stale the items it sees.
    // The shadow accumulates every node/edge seen across all polls in the
    // browser's history, so devices that have dropped out of CDP still
    // appear (gray) instead of vanishing across a page reload.
    var cached = shadowAsData();
    if (cached) {
      lastData = cached;
      render();
      if (cy) {
        cy.elements().addClass("stale");
        refreshStaleAlarms();
      }
      var status = document.getElementById("topology-status");
      if (status) status.textContent += " (cached, awaiting refresh…)";
    }
    load();
    if (autoToggle.checked) startAutoRefresh();
  };
})();
