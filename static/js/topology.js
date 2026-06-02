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

    // 3. Emit one port-child per (device, port) actually used by an edge,
    //    plus the port-anchored edges themselves.
    var emittedPorts = new Set();
    data.edges.forEach(function (e) {
      if (!visibleDevices.has(e.source) || !visibleDevices.has(e.target)) return;
      var srcPortId = portChildId(e.source, e.sport);
      var tgtPortId = portChildId(e.target, e.tport);
      if (!emittedPorts.has(srcPortId)) {
        emittedPorts.add(srcPortId);
        els.push({
          group: "nodes",
          data: { id: srcPortId, parent: e.source, label: e.sport || "?" },
          classes: "port",
        });
      }
      if (!emittedPorts.has(tgtPortId)) {
        emittedPorts.add(tgtPortId);
        els.push({
          group: "nodes",
          data: { id: tgtPortId, parent: e.target, label: e.tport || "?" },
          classes: "port",
        });
      }
      els.push({
        group: "edges",
        data: {
          id: e.id,
          source: srcPortId,
          target: tgtPortId,
          sport: e.sport,
          tport: e.tport,
          // Stash the device-level endpoints so renderDetail can compute
          // direction without re-walking the port hierarchy.
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
      {
        selector: "edge",
        style: {
          width: 1,
          "line-color": "#666",
          "curve-style": "bezier",
          "target-arrow-shape": "triangle",
          "target-arrow-color": "#666",
          "arrow-scale": 1.1,
        },
      },
      {
        selector: "edge:selected",
        style: {
          "line-color": "#2980b9",
          "target-arrow-color": "#2980b9",
          width: 2,
        },
      },
    ];
  }

  function layoutOptions(name) {
    var common = { name: name, animate: false, fit: true, padding: 40 };
    if (name === "fcose") {
      return Object.assign(common, {
        quality: "default",
        randomize: true,
        nodeSeparation: 80,
        idealEdgeLength: 120,
        nodeRepulsion: 8000,
        gravity: 0.2,
        gravityCompound: 1.5,
        numIter: 2500,
        tile: true,
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

  function fitAndLayout() {
    if (!cy) return;
    var name = document.getElementById("topology-layout").value;
    // Fall back to cose if fcose extension didn't load.
    if (name === "fcose" && !cy.layout(layoutOptions("fcose")).options) {
      name = "cose";
    }
    cy.layout(layoutOptions(name)).run();
    cy.fit(undefined, 30);
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
    cy.on("tap", "node", function (evt) { renderDetail(evt.target); });
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
