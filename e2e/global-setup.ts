import { request } from "@playwright/test";

// Seed test data after coordinator starts
export default async function globalSetup() {
  const baseURL = process.env.COORDINATOR_URL || "http://localhost:9990";

  // Wait for server to be ready
  const maxRetries = 10;
  let serverReady = false;

  for (let i = 0; i < maxRetries; i++) {
    try {
      const context = await request.newContext({ baseURL });
      const healthResponse = await context.get("/api/v1/health_check", {
        timeout: 2000,
      });
      await context.dispose();
      if (healthResponse.ok()) {
        serverReady = true;
        console.log("Server is ready");
        break;
      }
    } catch {
      console.log(`Waiting for server... (attempt ${i + 1}/${maxRetries})`);
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }

  if (!serverReady) {
    console.error("Server failed to start");
    return;
  }

  const context = await request.newContext({ baseURL });

  // Create a test competition via admin API
  const now = new Date();
  const startDate = new Date(now.getTime() + 60 * 60 * 1000); // 1 hour from now
  const endDate = new Date(now.getTime() + 2 * 60 * 60 * 1000); // 2 hours from now
  const signingDate = new Date(now.getTime() + 3 * 60 * 60 * 1000); // 3 hours from now

  const formData = new URLSearchParams();
  formData.append("id", crypto.randomUUID());
  formData.append("signing_date", signingDate.toISOString());
  formData.append("start_observation_date", startDate.toISOString());
  formData.append("end_observation_date", endDate.toISOString());
  formData.append("number_of_values_per_entry", "9");
  formData.append("total_allowed_entries", "10");
  formData.append("entry_fee", "1000");
  formData.append("coordinator_fee_percentage", "10");
  formData.append("number_of_places_win", "3");
  formData.append("locations", "KORD");
  formData.append("locations", "KJFK");
  formData.append("locations", "KLAX");

  try {
    const response = await context.post("/admin/api/competitions", {
      headers: {
        "Content-Type": "application/x-www-form-urlencoded",
      },
      data: formData.toString(),
      timeout: 10000,
    });

    const responseText = await response.text();
    if (responseText.includes("error") || responseText.includes("Error")) {
      console.error("Competition creation error:", responseText);
    } else {
      console.log("Seeded test competition successfully");
    }
  } catch (error) {
    console.error("Failed to seed competition (non-fatal):", error);
    // Don't throw - tests can still run with existing data or without a competition
  }

  await context.dispose();
}
