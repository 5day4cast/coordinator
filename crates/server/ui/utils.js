export async function hash_object(obj) {
  console.log("Pre erialized JSON in JS:", obj);
  console.log("Expected Observations:", obj.expected_observations);
  obj.expected_observations.forEach((obs, index) => {
    console.log(`Observation ${index}:`, Object.keys(obs));
  });
  const objString = JSON.stringify(obj, handle_array);
  console.log("Serialized JSON in JS:", objString);
  // Step 2: Convert the string to an ArrayBuffer using TextEncoder
  const encoder = new TextEncoder();
  const data = encoder.encode(objString);

  // Step 3: Generate the SHA-256 hash
  const hashBuffer = await crypto.subtle.digest("SHA-256", data);

  // Step 4: Convert the ArrayBuffer to a hexadecimal string
  const hashArray = Array.from(new Uint8Array(hashBuffer));
  const hashHex = hashArray
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");

  return hashHex;
}

function handle_array(key, value) {
  if (Array.isArray(value)) {
    return value;
  }
  return value;
}

export function one_day_ahead(rfc3339String) {
  const date = new Date(rfc3339String);
  date.setDate(date.getDate() + 1);
  const originalOffset = rfc3339String.match(/[-+]\d{2}:\d{2}/)[0];
  const updatedRfc3339String = date.toISOString().replace("Z", originalOffset);
  return updatedRfc3339String;
}
