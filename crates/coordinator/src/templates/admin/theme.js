// Admin-only preference; apply before paint without loading the public app bundle.
const key = "coordinator-admin-theme";
let preference = "dark";
try { preference = localStorage.getItem(key) || "dark"; } catch (_) {}
if (!["light", "dark"].includes(preference)) preference = "dark";
function apply() {
  document.documentElement.dataset.theme = preference;
  const select = document.getElementById("admin-theme");
  if (select) select.value = preference;
}
apply();
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
