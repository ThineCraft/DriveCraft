// Applies the chosen theme before the page paints. The choice is "system", "light"
// or "dark" and is remembered on this computer.
"use strict";

window.dcTheme = (function () {
  const KEY = "drivecraft-theme";
  const VALID = ["system", "light", "dark"];
  function get() {
    try {
      const v = localStorage.getItem(KEY);
      return VALID.includes(v) ? v : "system";
    } catch (e) {
      return "system";
    }
  }
  function apply(choice) {
    if (choice === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", choice);
  }
  function set(choice) {
    if (!VALID.includes(choice)) return;
    try { localStorage.setItem(KEY, choice); } catch (e) { /* still applies for this run */ }
    apply(choice);
  }
  apply(get());
  return { get, set };
})();
