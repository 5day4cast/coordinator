// Admin-only preference; apply before paint without loading the public app bundle.
const key = "coordinator-admin-theme";
const system = matchMedia("(prefers-color-scheme: dark)");
let preference = "system";
try { preference = localStorage.getItem(key) || "system"; } catch (_) {}
if (!["light", "dark", "system"].includes(preference)) preference = "system";
function apply() {
  document.documentElement.dataset.theme = preference === "system" ? (system.matches ? "dark" : "light") : preference;
  const select = document.getElementById("admin-theme");
  if (select) select.value = preference;
}
apply();
system.addEventListener("change", apply);
document.addEventListener("DOMContentLoaded", () => {
  const select = document.getElementById("admin-theme");
  select.closest("label").hidden = false;
  select.value = preference;
  select.addEventListener("change", () => {
    preference = select.value;
    try { localStorage.setItem(key, preference); } catch (_) {}
    apply();
  });
});
