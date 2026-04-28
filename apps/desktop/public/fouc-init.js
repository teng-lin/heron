(function () {
  try {
    var theme = localStorage.getItem("heron:theme");
    // Only "dark" forces dark; "system" (or missing/unknown) follows matchMedia.
    if (theme !== "dark" && theme !== "light") {
      theme = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
    }
    // "light" → no data-theme attribute (default); "dark" → set it.
    if (theme === "dark") {
      document.documentElement.dataset.theme = "dark";
    } else {
      delete document.documentElement.dataset.theme;
    }

    var accent = localStorage.getItem("heron:accent");
    // Only whitelisted values are accepted; anything else (incl. legacy "bronze") clears the attribute.
    if (accent === "ink" || accent === "heron" || accent === "sage") {
      document.documentElement.dataset.accent = accent;
    } else {
      delete document.documentElement.dataset.accent;
    }
  } catch (e) {}
})();
