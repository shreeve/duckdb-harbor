const THEMES = ["light", "dark", "paper", "midnight", "contrast"];
const saved = localStorage.getItem("ducktable-theme") || "light";
document.documentElement.dataset.theme = saved;
window.addEventListener("DOMContentLoaded", () => {
  const head = document.querySelector(".page-head");
  if (!head) return;
  const pick = document.createElement("select");
  pick.className = "theme-pick";
  for (const t of THEMES) {
    const o = document.createElement("option");
    o.value = t; o.textContent = "theme: " + t; o.selected = t === saved;
    pick.append(o);
  }
  pick.onchange = () => {
    document.documentElement.dataset.theme = pick.value;
    localStorage.setItem("ducktable-theme", pick.value);
  };
  head.append(pick);
});
