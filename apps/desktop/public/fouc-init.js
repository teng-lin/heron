(function () {
  try {
    var theme = localStorage.getItem("heron:theme");
    // Treat missing key and "system" identically: follow prefers-color-scheme.
    if (!theme || theme === "system") {
      theme = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
    }
    // "light" → no data-theme attribute (default); "dark" → set it.
    if (theme === "dark") {
      document.documentElement.dataset.theme = "dark";
    } else {
      delete document.documentElement.dataset.theme;
    }

    var accent = localStorage.getItem("heron:accent");
    if (accent) {
      document.documentElement.dataset.accent = accent;
    } else {
      delete document.documentElement.dataset.accent;
    }
  } catch (e) {}
})();
