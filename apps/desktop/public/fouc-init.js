(function () {
  try {
    var theme = localStorage.getItem("heron:theme");
    if (!theme) {
      theme = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
    }
    if (theme === "dark") document.documentElement.dataset.theme = "dark";

    var accent = localStorage.getItem("heron:accent");
    if (accent) document.documentElement.dataset.accent = accent;
  } catch (e) {}
})();
