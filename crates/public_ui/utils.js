
export function one_day_ahead(rfc3339String) {
  const date = new Date(rfc3339String);
  date.setDate(date.getDate() + 1);
  const originalOffset = rfc3339String.match(/[-+]\d{2}:\d{2}/)[0];
  const updatedRfc3339String = date.toISOString().replace("Z", originalOffset);
  return updatedRfc3339String;
}
