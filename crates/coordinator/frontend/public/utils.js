export function one_day_ahead(rfc3339String) {
  const date = new Date(rfc3339String);
  date.setDate(date.getDate() + 1);
  console.log(rfc3339String);

  // Check if the string uses 'Z' for UTC or has an explicit offset
  let updatedRfc3339String;
  if (rfc3339String.endsWith("Z")) {
    // Keep the 'Z' format for UTC time
    updatedRfc3339String = date.toISOString(); // Already has Z
  } else {
    // Extract the original offset and replace the Z in toISOString()
    const offsetMatch = rfc3339String.match(/[-+]\d{2}:\d{2}/);
    const originalOffset = offsetMatch ? offsetMatch[0] : "+00:00";
    updatedRfc3339String = date.toISOString().replace("Z", originalOffset);
  }
  return updatedRfc3339String;
}
