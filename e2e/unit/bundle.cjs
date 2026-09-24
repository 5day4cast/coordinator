// Runs template scripts the way build.rs bundles them: inside one function,
// so they share top-level names with each other but put nothing on window.
// Names from other scripts come in through `globals`; `exports` names what
// the caller gets back.
const { readFileSync } = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const TEMPLATES = path.join(__dirname, "../../crates/coordinator/src/templates");

function loadBundle(files, globals, exports) {
  const source = files
    .map((file) => readFileSync(path.join(TEMPLATES, file), "utf8"))
    .join("\n;\n");
  return vm.runInNewContext(`(() => {\n${source}\n;return { ${exports.join(", ")} };\n})()`, globals);
}

module.exports = { loadBundle, TEMPLATES };
