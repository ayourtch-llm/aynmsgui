// Drag-to-resize column widths on a table that already has fixed widths
// (typically pinned by quicksearch.js).
//
//   initColumnResize("my-table-id")
//
// Adds a thin "splitter" handle on the right edge of every <th> except the
// last. Dragging steals pixels from the next column, so the table's overall
// width stays constant. State is not persisted — widths reset on reload.
(function () {
  var MIN_WIDTH = 30;

  function attachDragHandler(handle, th, nextTh) {
    handle.addEventListener("mousedown", function (e) {
      e.preventDefault();
      var startX = e.clientX;
      var startWidth = th.offsetWidth;
      var startNextWidth = nextTh.offsetWidth;

      function onMove(ev) {
        var delta = ev.clientX - startX;
        // Clamp so both columns stay >= MIN_WIDTH
        var maxDelta = startNextWidth - MIN_WIDTH;
        var minDelta = -(startWidth - MIN_WIDTH);
        if (delta > maxDelta) delta = maxDelta;
        if (delta < minDelta) delta = minDelta;
        th.style.width = (startWidth + delta) + "px";
        nextTh.style.width = (startNextWidth - delta) + "px";
      }
      function onUp() {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      }
      document.body.style.cursor = "col-resize";
      // Suppress text selection while dragging — otherwise the browser
      // selects table cell contents as the mouse moves.
      document.body.style.userSelect = "none";
      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    });
  }

  window.initColumnResize = function (tableId) {
    var table = document.getElementById(tableId);
    if (!table) {
      console.warn("colresize: no table with id", tableId);
      return;
    }
    var headerRow = (table.tHead && table.tHead.rows.length)
      ? table.tHead.rows[0]
      : (table.rows.length ? table.rows[0] : null);
    if (!headerRow) return;

    var ths = headerRow.cells;
    // Skip the last column — there's nothing on its right to steal from.
    for (var i = 0; i < ths.length - 1; i++) {
      var th = ths[i];
      var nextTh = ths[i + 1];
      // The handle is absolutely positioned within the th.
      if (getComputedStyle(th).position === "static") {
        th.style.position = "relative";
      }
      var handle = document.createElement("span");
      handle.className = "col-resize-handle";
      th.appendChild(handle);
      attachDragHandler(handle, th, nextTh);
    }
  };
})();
