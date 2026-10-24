// Links back to the main site (meetparley.app): the guide's title, a home
// button at the start of the top bar, and a link at the top of the sidebar.
(function () {
  var home = "https://meetparley.app/";
  var title = document.querySelector(".menu-title");
  if (title && !title.querySelector("a")) {
    var link = document.createElement("a");
    link.href = home;
    link.className = "parley-home-title";
    link.title = "Back to meetparley.app";
    link.textContent = title.textContent;
    title.textContent = "";
    title.appendChild(link);
  }
  var buttons = document.querySelector(".left-buttons");
  if (buttons) {
    var button = document.createElement("a");
    button.href = home;
    button.className = "icon-button parley-home-button";
    button.title = "Back to meetparley.app";
    button.setAttribute("aria-label", "Back to meetparley.app");
    button.innerHTML =
      '<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 11l9-8 9 8"/><path d="M5 10v10h14V10"/></svg>';
    buttons.insertBefore(button, buttons.firstChild);
  }
  var sidebar = document.querySelector("#mdbook-sidebar .sidebar-scrollbox");
  if (sidebar && sidebar.parentNode) {
    var back = document.createElement("a");
    back.href = home;
    back.className = "parley-sidebar-home";
    back.textContent = "\u2190 meetparley.app";
    sidebar.parentNode.insertBefore(back, sidebar);
  }
})();
