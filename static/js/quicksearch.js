// Client-side row filter: 4 inputs, AND semantics across all 4.
//
//   initQuickSearch("my-table-id")
//   initQuickSearch("my-table-id", { inputCount: 4, headerClass: "header", counterId: "totalMatches" })
//
// Each input matches against the row's innerHTML. Prefix with "^" to NOT-contain.
// Lowercase queries are case-insensitive; queries with any uppercase character
// are case-sensitive. Rows with class === options.headerClass are never hidden.
//
// Ported from the rspten interface (/tmp/assets.html).
(function () {
  function hasText(haystack, q) {
    if (q === "") return true;
    var lowerQ = q.toLowerCase();
    if (q === lowerQ) return haystack.toLowerCase().indexOf(lowerQ) !== -1;
    return haystack.indexOf(q) !== -1;
  }

  function notHasText(haystack, q) {
    if (q === "") return true;
    var lowerQ = q.toLowerCase();
    if (q === lowerQ) return haystack.toLowerCase().indexOf(lowerQ) === -1;
    return haystack.indexOf(q) === -1;
  }

  function alwaysTrue() {
    return true;
  }

  function compileQuery(raw) {
    if (!raw) return { fn: alwaysTrue, q: "" };
    if (raw.charAt(0) === "^") return { fn: notHasText, q: raw.slice(1) };
    return { fn: hasText, q: raw };
  }

  function makeHandler(table, inputs, counter, headerClass) {
    return function () {
      var compiled = inputs.map(function (el) {
        return compileQuery(el.value);
      });

      // Detach the tbody before mutating row.style.display per row.
      // In an unparented subtree the browser doesn't lay out / repaint, so
      // hundreds of style writes collapse into a single reflow on reattach
      // instead of one reflow per row.
      var tbody = table.tBodies.length ? table.tBodies[0] : null;
      var rows;
      var parent = null;
      var nextSibling = null;
      if (tbody) {
        parent = tbody.parentNode;
        nextSibling = tbody.nextSibling;
        parent.removeChild(tbody);
        rows = tbody.rows;
      } else {
        rows = table.rows;
      }

      var matches = 0;
      for (var i = 0; i < rows.length; i++) {
        var row = rows[i];
        if (row.className === headerClass) continue;
        var haystack = row.innerHTML;
        var keep = true;
        for (var j = 0; j < compiled.length; j++) {
          if (!compiled[j].fn(haystack, compiled[j].q)) {
            keep = false;
            break;
          }
        }
        if (keep) {
          row.style.display = "";
          matches++;
        } else {
          row.style.display = "none";
        }
      }

      if (tbody) {
        parent.insertBefore(tbody, nextSibling);
      }

      if (counter) counter.textContent = "Total matches: " + matches;
    };
  }

  // Pin column widths to whatever the browser laid out for the header row,
  // then switch the table to `table-layout: fixed` so columns no longer
  // recompute as rows hide/show during filtering. Without this, every
  // keystroke can shift column widths (a noticeable shimmy on wide tables).
  function freezeColumnWidths(table) {
    var headerRow = null;
    if (table.tHead && table.tHead.rows.length) {
      headerRow = table.tHead.rows[0];
    } else if (table.rows.length) {
      headerRow = table.rows[0];
    }
    if (!headerRow) return;

    var ths = headerRow.cells;
    // Snapshot first, then apply — measuring after each write would give
    // distorted values once table-layout flips.
    var widths = [];
    for (var i = 0; i < ths.length; i++) {
      widths.push(ths[i].offsetWidth);
    }
    for (var j = 0; j < ths.length; j++) {
      ths[j].style.boxSizing = "border-box";
      ths[j].style.width = widths[j] + "px";
    }
    table.style.tableLayout = "fixed";
  }

  window.initQuickSearch = function (tableId, opts) {
    opts = opts || {};
    var inputCount = opts.inputCount || 4;
    var headerClass = opts.headerClass || "header";
    var counterId = opts.counterId || "totalMatches";
    var idPrefix = opts.idPrefix || "quickSearch";

    var table = document.getElementById(tableId);
    if (!table) {
      console.warn("quicksearch: no table with id", tableId);
      return;
    }
    var inputs = [];
    for (var i = 1; i <= inputCount; i++) {
      var el = document.getElementById(idPrefix + i);
      if (el) inputs.push(el);
    }
    if (!inputs.length) {
      console.warn("quicksearch: no inputs found for prefix", idPrefix);
      return;
    }

    // Freeze column widths once, before any user interaction.
    freezeColumnWidths(table);

    var counter = document.getElementById(counterId);
    var handler = makeHandler(table, inputs, counter, headerClass);
    inputs.forEach(function (el) {
      el.oninput = handler;
    });
    if (inputs[0]) inputs[0].focus();
  };
})();
