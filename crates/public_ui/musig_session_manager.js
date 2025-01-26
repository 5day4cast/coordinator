class MusigSessionManager {
  constructor(
    wallet,
    competitionId,
    entryId,
    client,
    entryIndex,
    onStateChange = null,
  ) {
    this.wallet = wallet;
    this.competitionId = competitionId;
    this.entryId = entryId;
    this.client = client;
    this.state = "INITIALIZED";
    console.log(entryIndex);
    this.entryIndex = entryIndex;
    this.contractParams = null;
    this.fundingOutpoint = null;
    this.onStateChange = onStateChange;
    this.lastError = null;
    if (this.onStateChange) {
      this.onStateChange(this);
    }
  }

  // State machine transitions
  async start() {
    if (this.state !== "INITIALIZED") {
      throw new Error("Invalid state transition");
    }

    // Begin polling for contract parameters
    await this.pollForContractParameters();
  }

  async pollWithBackoff(pollingConfig) {
    const {
      initialState,
      endpoint,
      processResponse,
      baseDelay = 5000,
      maxDelay = 30000,
      description = "resource",
    } = pollingConfig;

    console.log(`Polling for ${description}...`);
    console.log(`Current state: ${this.state}, expecting: ${initialState}`);
    let attempt = 0;

    while (this.state === initialState) {
      try {
        const response = await this.client.get(endpoint);

        if (response.ok) {
          await processResponse(response);
          break;
        } else if (response.status === 404) {
          const delay = Math.min(baseDelay * Math.pow(1.5, attempt), maxDelay);
          console.log(
            `${description} not ready yet, waiting ${delay / 1000} seconds before next attempt...`,
          );
          await new Promise((resolve) => setTimeout(resolve, delay));
          attempt++;
        } else {
          throw new Error(`Unexpected response: ${response.status}`);
        }
      } catch (error) {
        console.error(`Error polling ${description}:`, error);
        if (!error.message || !error.message.includes("404")) {
          this.setState("ERROR", error);
          break;
        }

        const delay = Math.min(baseDelay * Math.pow(1.5, attempt), maxDelay);
        console.log(
          `Error encountered, waiting ${delay / 1000} seconds before retry...`,
        );
        await new Promise((resolve) => setTimeout(resolve, delay));
        attempt++;
      }
    }
  }

  async pollForContractParameters() {
    await this.pollWithBackoff({
      initialState: "INITIALIZED",
      endpoint: `${this.client.apiBase}/competitions/${this.competitionId}/contract`,
      description: "contract parameters",
      processResponse: async (response) => {
        const fundedContract = await response.json();
        console.log("funded contract:", fundedContract);
        this.contractParams = fundedContract.contract_params;
        this.fundingOutpoint = fundedContract.funding_outpoint;
        console.log(this.contractParams);
        this.setState("CONTRACT_RECEIVED");
        await this.handleContractReceived();
      },
    });
  }

  async handleContractReceived() {
    console.log("Contract parameters received, adding contract to wallet...");

    try {
      console.log(this.entryIndex);
      console.log(this.contractParams);
      console.log(this.fundingOutpoint);
      console.log(this.contractParams);
      await this.wallet.addEntryIndex(this.entryIndex);

      const transformedContractParams = {
        ...this.contractParams,
        outcome_payouts: transformOutcomePayouts(
          this.contractParams.outcome_payouts,
        ),
      };
      console.log(transformedContractParams);

      // Add contract to wallet
      await this.wallet.addContract(
        this.entryIndex,
        transformedContractParams,
        this.fundingOutpoint,
      );
      console.log(`entry_index ${this.entryIndex}`);

      // Generate and submit nonces
      const nonces = await this.wallet.generatePublicNonces(this.entryIndex);
      console.log("Generated nonces full structure:", {
        by_outcome: nonces.by_outcome,
        by_win_condition: nonces.by_win_condition,
      });

      if (!nonces || !nonces.by_outcome || !nonces.by_win_condition) {
        throw new Error("Invalid nonce structure");
      }

      // Verify that the Maps have entries
      if (
        !(nonces.by_outcome instanceof Map) ||
        !(nonces.by_win_condition instanceof Map)
      ) {
        throw new Error("Nonce collections are not Map objects");
      }

      if (nonces.by_outcome.size === 0 || nonces.by_win_condition.size === 0) {
        throw new Error("One or both nonce collections are empty");
      }

      // Convert Maps to plain objects for JSON serialization
      const serializedNonces = {
        by_outcome: Object.fromEntries(nonces.by_outcome),
        by_win_condition: Object.fromEntries(nonces.by_win_condition),
      };
      console.log(
        "Sending nonces to server:",
        JSON.stringify(serializedNonces, null, 2),
      );

      await this.client.post(
        `${this.client.apiBase}/competitions/${this.competitionId}/entries/${this.entryId}/public_nonces`,
        serializedNonces,
      );

      this.setState("NONCES_SUBMITTED");

      await this.pollForAggregateNonces();
    } catch (error) {
      console.error("Error handling contract:", error);
      this.setState("ERROR", error);

      throw error;
    }
  }

  async pollForAggregateNonces() {
    await this.pollWithBackoff({
      initialState: "NONCES_SUBMITTED",
      endpoint: `${this.client.apiBase}/competitions/${this.competitionId}/aggregate_nonces`,
      description: "aggregate nonces",
      processResponse: async (response) => {
        const aggregateNonces = await response.json();
        console.log("Received aggregate nonces:", aggregateNonces);
        await this.handleAggregateNonces(aggregateNonces);
      },
    });
  }

  async handleAggregateNonces(aggregateNonces) {
    console.log(
      "Handling aggregate nonces with contract params:",
      this.contractParams,
    );
    console.log("Funding outpoint:", this.fundingOutpoint);
    console.log("Raw aggregate nonces:", aggregateNonces);

    try {
      // Ensure contract is set up before signing
      await this.wallet.addEntryIndex(this.entryIndex);

      const transformedContractParams = {
        ...this.contractParams,
        outcome_payouts: transformOutcomePayouts(
          this.contractParams.outcome_payouts,
        ),
      };

      // Re-add contract before signing - this should be idempotent
      await this.wallet.addContract(
        this.entryIndex,
        transformedContractParams,
        this.fundingOutpoint,
      );

      const transformedAggregateNonces = {
        by_outcome: new Map(Object.entries(aggregateNonces.by_outcome)),
        by_win_condition: new Map(
          Object.entries(aggregateNonces.by_win_condition),
        ),
      };

      console.log("Generated aggregate full structure:", {
        by_outcome: transformedAggregateNonces.by_outcome,
        by_win_condition: transformedAggregateNonces.by_win_condition,
      });

      if (
        !transformedAggregateNonces ||
        !transformedAggregateNonces.by_outcome ||
        !transformedAggregateNonces.by_win_condition
      ) {
        throw new Error("Invalid aggregate nonce structure");
      }

      // Verify that the Maps have entries
      if (
        !(transformedAggregateNonces.by_outcome instanceof Map) ||
        !(transformedAggregateNonces.by_win_condition instanceof Map)
      ) {
        throw new Error("Aggregate collections are not Map objects");
      }

      if (
        transformedAggregateNonces.by_outcome.size === 0 ||
        transformedAggregateNonces.by_win_condition.size === 0
      ) {
        throw new Error("One or both Aggregate collections are empty");
      }

      // Generate partial signatures using aggregate nonces
      const partialSigs = await this.wallet.signAggregateNonces(
        transformedAggregateNonces,
        this.entryIndex,
      );

      console.log("Generated partialSigs full structure:", {
        by_outcome: partialSigs.by_outcome,
        by_win_condition: partialSigs.by_win_condition,
      });

      if (
        !partialSigs ||
        !partialSigs.by_outcome ||
        !partialSigs.by_win_condition
      ) {
        throw new Error("Invalid partialSigs structure");
      }

      // Verify that the Maps have entries
      if (
        !(partialSigs.by_outcome instanceof Map) ||
        !(partialSigs.by_win_condition instanceof Map)
      ) {
        throw new Error("PartialSigs collections are not Map objects");
      }

      if (
        partialSigs.by_outcome.size === 0 ||
        partialSigs.by_win_condition.size === 0
      ) {
        throw new Error("One or both partialSigs collections are empty");
      }

      const serializedPartialSigs = {
        by_outcome: Object.fromEntries(partialSigs.by_outcome),
        by_win_condition: Object.fromEntries(partialSigs.by_win_condition),
      };

      console.log(
        "Sending partialSigs to server:",
        JSON.stringify(serializedPartialSigs, null, 2),
      );

      await this.client.post(
        `${this.client.apiBase}/competitions/${this.competitionId}/entries/${this.entryId}/partial_signatures`,
        serializedPartialSigs,
      );

      this.setState("COMPLETED");
      console.log("User's musig session completed successfully");
    } catch (error) {
      console.error("Error handling aggregate nonces:", error);
      this.setState("ERROR", error);

      throw error;
    }
  }

  getState() {
    console.log(`Getting state for session ${this.entryId}:`, this.state);
    return this.state;
  }

  setState(newState, error = null) {
    console.log(`Setting state for session ${this.entryId}:`, newState);
    this.state = newState;
    this.lastError = error;

    if (this.onStateChange) {
      this.onStateChange(this);
    }
  }
}

const transformOutcomePayouts = (originalPayouts) => {
  const transformed = {};

  for (const [key, valueObj] of Object.entries(originalPayouts)) {
    // Convert the inner object to a Map
    const valueMap = new Map(
      Object.entries(valueObj).map(([k, v]) => [Number(k), v]),
    );

    transformed[key] = valueMap;
  }

  return transformed;
};

export { MusigSessionManager };
