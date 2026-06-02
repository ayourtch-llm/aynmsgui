// Drag-to-resize column widths on a table that already has fixed widths
// (typically pinned by quicksearch.js).
//
//   initColumnResize("my-table-id")
//
// Adds a "splitter" handle on the right edge of every <th>. Dragging
// resizes *only* the dragged column — the table grows or shrinks
// horizontally to match, so other columns stay put. The wrapping
// container should allow horizontal overflow (e.g. overflow-x: auto on
// .main) so users can scroll to reach widened columns.
//
// State is not persisted — widths reset on reload.
(function () {
  var MIN_WIDTH = 30;

  function attachDragHandler(handle, th, table) {
    handle.addEventListener("mousedown", function (e) {
      e.preventDefault();
      var startX = e.clientX;
      var startWidth = th.offsetWidth;

      // Pin the table to its current pixel width so growing one column
      // grows the total table, instead of the browser redistributing the
      // delta across other columns to keep width: 100% satisfied.
      if (!table.style.width || table.style.width.indexOf("px") === -1) {
        table.style.width = table.offsetWidth + "px";
      }
      var startTableWidth = table.offsetWidth;

      function onMove(ev) {
        var delta = ev.clientX - startX;
        if (delta < -(startWidth - MIN_WIDTH)) delta = -(startWidth - MIN_WIDTH);
        th.style.width = (startWidth + delta) + "px";
        table.style.width = (startTableWidth + delta) + "px";
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
    for (var i = 0; i < ths.length; i++) {
      var th = ths[i];
      // The handle is absolutely positioned within the th.
      if (getComputedStyle(th).position === "static") {
        th.style.position = "relative";
      }
      var handle = document.createElement("span");
      handle.className = "col-resize-handle";
      th.appendChild(handle);
      attachDragHandler(handle, th, table);
    }
  };
})();
