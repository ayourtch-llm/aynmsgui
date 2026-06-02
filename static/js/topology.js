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
    var parts = [];
    parts.push('<h3 style="margin:0 0 8px 0;">' + escapeHtml(d.label || d.id) + "</h3>");
    if (d.managed) {
      parts.push('<p><span style="background:#e0e0e0; padding:1px 6px; border-radius:3px;">managed</span></p>');
    }
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
    var deviceId = d.id;
    var deviceNode = cy.getElementById(deviceId);
    var edges = deviceNode.children().connectedEdges();
    if (edges.length) {
      parts.push("<h4 style='margin:12px 0 4px 0;'>Adjacencies (" + edges.length + ")</h4><ul style='padding-left:18px; margin:0;'>");
      edges.forEach(function (e) {
        var ed = e.data();
        var direction = ed._sourceDevice === deviceId ? "→" : "←";
        var localPort = ed._sourceDevice === deviceId ? ed.sport : ed.tport;
        var remotePort = ed._sourceDevice === deviceId ? ed.tport : ed.sport;
        var other = ed._sourceDevice === deviceId ? ed._targetDevice : ed._sourceDevice;
        parts.push("<li><code>" + escapeHtml(localPort || "?") + "</code> " +
          direction + " <strong>" + escapeHtml(other) + "</strong>" +
          " <code>" + escapeHtml(remotePort || "?") + "</code></li>");
      });
      parts.push("</ul>");
    }
    document.getElementById("topology-detail").innerHTML = parts.join("");
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
          label: "data(label)",
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
  }

  function load() {
    var status = document.getElementById("topology-status");
    status.textContent = "loading…";
    fetch("/topology/json")
      .then(function (r) { return r.json(); })
      .then(function (data) {
        lastData = data;
        render();
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
        cy.add(el);
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
        existing.removeClass("stale");
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
    alignChildrenInColumns();

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
  }

  function autoLoad() {
    fetch("/topology/json")
      .then(function (r) { return r.json(); })
      .then(function (data) { mergeUpdate(data); })
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

  window.initTopology = function () {
    document.getElementById("topology-refresh").addEventListener("click", load);
    document.getElementById("topology-reset-layout").addEventListener("click", resetLayout);
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
    load();
    if (autoToggle.checked) startAutoRefresh();
  };
})();
