/*
 * Progressive enhancement for the documentation site. Everything here is an
 * improvement on a page that already reads correctly without it: the theme
 * toggle, headings that can be linked to, the "on this page" column, captions
 * under the animations, and tables that scroll rather than overflow.
 */

(function () {
  "use strict";

  /* ------------------------------------------------------------- theming */

  var root = document.documentElement;
  var toggle = document.getElementById("theme-toggle");

  function effectiveTheme() {
    var chosen = root.getAttribute("data-theme");
    if (chosen === "light" || chosen === "dark") {
      return chosen;
    }
    return window.matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
  }

  function describeToggle() {
    if (!toggle) {
      return;
    }
    var next = effectiveTheme() === "dark" ? "light" : "dark";
    toggle.setAttribute("aria-label", "Switch to the " + next + " theme");
    toggle.setAttribute("title", "Switch to the " + next + " theme");
  }

  if (toggle) {
    describeToggle();
    toggle.addEventListener("click", function () {
      var next = effectiveTheme() === "dark" ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try {
        localStorage.setItem("deco-theme", next);
      } catch (e) {
        /* A browser that refuses storage still gets the switch, just not
           the memory of it. */
      }
      describeToggle();
    });
  }

  window
    .matchMedia("(prefers-color-scheme: light)")
    .addEventListener("change", describeToggle);

  /* ------------------------------------------------- figures and tables */

  var prose = document.querySelector(".prose");
  if (!prose) {
    return;
  }

  // An image alone in a paragraph is one of the generated animations; its alt
  // text says what is being demonstrated, which is exactly the caption.
  Array.prototype.forEach.call(prose.querySelectorAll("p > img"), function (img) {
    var paragraph = img.parentNode;
    if (paragraph.childElementCount !== 1 || paragraph.textContent.trim()) {
      return;
    }
    var figure = document.createElement("figure");
    figure.appendChild(img.cloneNode(true));
    if (img.alt) {
      var caption = document.createElement("figcaption");
      caption.textContent = img.alt;
      figure.appendChild(caption);
    }
    paragraph.parentNode.replaceChild(figure, paragraph);
  });

  // The key tables are wide; let them scroll inside the column rather than
  // widening the page.
  Array.prototype.forEach.call(prose.querySelectorAll("table"), function (table) {
    var wrapper = document.createElement("div");
    wrapper.className = "table-scroll";
    wrapper.setAttribute("tabindex", "0");
    table.parentNode.insertBefore(wrapper, table);
    wrapper.appendChild(table);
  });

  /* --------------------------------------------------- headings and TOC */

  var headings = prose.querySelectorAll("h2[id], h3[id]");
  Array.prototype.forEach.call(headings, function (heading) {
    var anchor = document.createElement("a");
    anchor.className = "heading-anchor";
    anchor.href = "#" + heading.id;
    anchor.textContent = "#";
    anchor.setAttribute("aria-label", "Link to this section");
    heading.appendChild(anchor);
  });

  var toc = document.getElementById("toc");
  if (!toc || headings.length < 2) {
    return;
  }

  var list = document.createElement("ul");
  var links = [];
  Array.prototype.forEach.call(headings, function (heading) {
    var item = document.createElement("li");
    item.className = "toc-" + heading.tagName.toLowerCase();
    var link = document.createElement("a");
    link.href = "#" + heading.id;
    // The anchor added above is part of the heading's text by now.
    link.textContent = heading.textContent.replace(/#$/, "").trim();
    item.appendChild(link);
    list.appendChild(item);
    links.push(link);
  });

  var title = document.createElement("h2");
  title.textContent = "On this page";
  toc.appendChild(title);
  toc.appendChild(list);
  toc.hidden = false;

  if (!("IntersectionObserver" in window)) {
    return;
  }

  // Whichever heading was passed last going down the page is the one the
  // reader is under.
  var visible = new Set();
  var observer = new IntersectionObserver(
    function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          visible.add(entry.target.id);
        } else {
          visible.delete(entry.target.id);
        }
      });

      var currentId = null;
      Array.prototype.forEach.call(headings, function (heading) {
        if (visible.has(heading.id) && currentId === null) {
          currentId = heading.id;
        }
      });

      links.forEach(function (link) {
        link.classList.toggle(
          "is-current",
          currentId !== null && link.hash === "#" + currentId
        );
      });
    },
    { rootMargin: "-72px 0px -70% 0px" }
  );

  Array.prototype.forEach.call(headings, function (heading) {
    observer.observe(heading);
  });
})();
