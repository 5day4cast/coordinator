import { displayCompetitions } from "./competitions.js";

const apiBase = API_BASE;
console.log("api location:", apiBase);
const oracleBase = ORACLE_BASE;
console.log("oracle location:", oracleBase)
displayCompetitions(oracleBase);