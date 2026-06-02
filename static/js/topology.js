// Topology graph powered by Cytoscape.js.
//
//   initTopology()  — fetch /topology/json once and render.
//
// Node styling:
//   .managed   → gray fill (matches the previous graphviz convention)
//   default    → white fill, gray border
// Edge:
//   triangle arrowhead at target end (source SEES target)
//   source-label / target-label render the local/remote port near each end
//
// Clicking a node populates the side panel with aux info (description,
// role, ip, platform, version) plus a link to /devices/<name> for
// managed devices.

(function () {
  var cy = null;

  function escapeHtml(s) {
    return String(s || "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function renderDetail(node) {
    var d = node.data();
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
    var edges = node.connectedEdges();
    if (edges.length) {
      parts.push("<h4 style='margin:12px 0 4px 0;'>Adjacencies (" + edges.length + ")</h4><ul style='padding-left:18px; margin:0;'>");
      edges.forEach(function (e) {
        var ed = e.data();
        var otherId = ed.source === d.id ? ed.target : ed.source;
        var direction = ed.source === d.id ? "→" : "←";
        var localPort = ed.source === d.id ? ed.sport : ed.tport;
        var remotePort = ed.source === d.id ? ed.tport : ed.sport;
        parts.push("<li><code>" + escapeHtml(localPort || "?") + "</code> " +
          direction + " <strong>" + escapeHtml(otherId) + "</strong>" +
          " <code>" + escapeHtml(remotePort || "?") + "</code></li>");
      });
      parts.push("</ul>");
    }
    document.getElementById("topology-detail").innerHTML = parts.join("");
  }

  function buildElements(data, managedOnly) {
    var els = [];
    var visibleIds = new Set();
    data.nodes.forEach(function (n) {
      if (managedOnly && !n.managed) return;
      visibleIds.add(n.id);
      els.push({
        group: "nodes",
        data: n,
        classes: n.managed ? "managed" : "unmanaged",
      });
    });
    data.edges.forEach(function (e) {
      if (!visibleIds.has(e.source) || !visibleIds.has(e.target)) return;
      els.push({ group: "edges", data: e });
    });
    return els;
  }

  function styles() {
    return [
      {
        selector: "node",
        style: {
          shape: "round-rectangle",
          "background-color": "#ffffff",
          "border-color": "#888",
          "border-width": 1,
          label: "data(label)",
          "text-valign": "center",
          "text-halign": "center",
          "font-size": "12px",
          "font-family": "Verdana, sans-serif",
          padding: "8px",
          width: "label",
          height: "label",
        },
      },
      {
        selector: "node.managed",
        style: {
          "background-color": "#e0e0e0",
          "border-color": "#444",
        },
      },
      {
        selector: "node:selected",
        style: {
          "border-color": "#2980b9",
          "border-width": 3,
        },
      },
      {
        selector: "edge",
        style: {
          width: 1,
          "line-color": "#666",
          "curve-style": "bezier",
          "target-arrow-shape": "triangle",
          "target-arrow-color": "#666",
          "arrow-scale": 1.2,
          "source-label": "data(sport)",
          "target-label": "data(tport)",
          "font-size": "8px",
          "font-family": "monospace",
          color: "#444",
          "text-background-color": "#fff",
          "text-background-opacity": 0.85,
          "text-background-padding": "1px",
          "source-text-offset": 18,
          "target-text-offset": 18,
        },
      },
    ];
  }

  function layoutOptions(name) {
    var common = { name: name, animate: false, fit: true, padding: 40 };
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
    cy.layout(layoutOptions(name)).run();
    cy.fit(undefined, 30);
  }

  var lastData = null;

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
    var visibleNodes = cy.nodes().length;
    var visibleEdges = cy.edges().length;
    status.textContent =
      visibleNodes + "/" + lastData.node_count + " nodes · " +
      visibleEdges + "/" + lastData.edge_count + " edges · CDP sweep: " + when;
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
