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

    // 4. Emit one edge per CDP adjacency. Bidirectional links naturally
    //    become two parallel edges (A→B and B→A); Cytoscape's bezier
    //    curve-style auto-offsets multiple edges between the same node
    //    pair into side-by-side curves. Each curve carries a single
    //    mid-arrow pointing toward its own target — i.e. arrows point
    //    OUTWARD at their respective ports, on separate parallel lines.
    data.edges.forEach(function (e) {
      if (!visibleDevices.has(e.source) || !visibleDevices.has(e.target)) return;
      els.push({
        group: "edges",
        data: {
          id: e.id,
          source: portChildId(e.source, e.sport),
          target: portChildId(e.target, e.tport),
          sport: e.sport,
          tport: e.tport,
          _sourceDevice: e.source,
          _targetDevice: e.target,
        },
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
      // Line runs all the way to the port box; the arrow triangle floats
      // along the line via mid-target-arrow (positioned ~75% along the
      // edge, pointing toward target). For bidirectional CDP adjacencies,
      // we emit two edges (A→B and B→A) which Cytoscape auto-offsets into
      // parallel curves — each curve has its own mid-arrow pointing
      // outward at its own target port, no in-line collision.
      {
        selector: "edge",
        style: {
          width: 1,
          "line-color": "#666",
          "curve-style": "bezier",
          "mid-target-arrow-shape": "triangle",
          "mid-target-arrow-color": "#666",
          "arrow-scale": 1.4,
          "source-endpoint": "outside-to-line",
          "target-endpoint": "outside-to-line",
          "source-distance-from-node": 1,
          "target-distance-from-node": 1,
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
      // Highlight: thicker blue line + matching blue mid-arrow.
      {
        selector: "edge.highlighted",
        style: {
          "line-color": "#2980b9",
          "mid-target-arrow-color": "#2980b9",
          width: 3,
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
    cy.layout(layoutOptions(name)).run();
    if (name === "fcose" || name === "cose") {
      alignChildrenInColumns();
    }
    cy.fit(undefined, 30);
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
    cy.on("select", "node", function (evt) {
      renderDetail(evt.target);
      highlightSiblings(evt.target);
    });
    cy.on("select", "edge", function (evt) {
      highlightSiblings(evt.target);
    });
    cy.on("unselect", function () {
      cy.elements(".highlighted").removeClass("highlighted");
    });
    fitAndLayout();
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

  window.initTopology = function () {
    document.getElementById("topology-refresh").addEventListener("click", load);
    document.getElementById("topology-managed-only").addEventListener("change", render);
    document.getElementById("topology-layout").addEventListener("change", render);
    load();
  };
})();
