class SigningProgressUI {
  constructor() {
    console.log("SigningProgressUI constructor called");
    this.visible = false;
    this.createUI();
    const signingStatus = document.getElementById("signingStatus");
    if (signingStatus) {
      signingStatus.classList.add("hidden");
    }

    if (this.container) {
      this.container.classList.add("is-hidden");
      this.container.classList.remove("is-visible");
    }

    this.activeSessionsCount = 0;
    this.setupToggleButton();
  }

  setRegistry(registry) {
    this.registry = registry;
  }

  createUI() {
    this.container = document.getElementById("signingProgressContainer");
    console.log("Container element:", this.container);

    if (!this.container) {
      console.error("Signing progress container not found in DOM");
      return;
    }

    // Set initial classes and styles
    this.container.className = "signing-progress-container";
    this.container.style.cssText = `
          background: white;
          border-radius: 6px;
          padding: 20px;
          margin-top: 20px;
          box-shadow: 0 2px 3px rgba(10, 10, 10, 0.1);
      `;

    // Add a header to the container
    const header = document.createElement("h3");
    header.className = "title is-4 mb-4";
    header.textContent = "Signing Progress";
    this.container.appendChild(header);

    this.verifyDOMStructure();
  }

  setupToggleButton() {
    this.toggleButton = document.getElementById("signingStatusNavClick");
    this.activeCountBadge = document.getElementById("activeSigningCount");

    // Remove the click handler from here since it's handled in navbar.js

    // Add close button to container
    const closeButton = document.createElement("button");
    closeButton.className = "delete is-small";
    closeButton.style.cssText = `
          position: absolute;
          top: 10px;
          right: 10px;
      `;
    closeButton.addEventListener("click", () => this.hide());
    this.container.appendChild(closeButton);
  }

  toggleVisibility() {
    console.log(
      "toggleVisibility called, current visible state:",
      this.visible,
    );
    if (this.visible) {
      this.hide();
    } else {
      this.show();
    }
  }

  show() {
    console.log("show() called");
    // Always show when explicitly called
    this.visible = true;

    const signingStatus = document.getElementById("signingStatus");
    if (signingStatus) {
      signingStatus.classList.remove("hidden");
      console.log("Removed hidden class from signingStatus");
    }

    if (this.container) {
      this.container.classList.remove("is-hidden");
      console.log("Removed is-hidden class from container");

      // Force a reflow
      void this.container.offsetHeight;

      this.container.classList.add("is-visible");
      console.log("Added is-visible class to container");

      // Update content
      this.updateUI();
    }
  }

  hide() {
    console.log("hide() called");
    this.visible = false;

    if (this.container) {
      this.container.classList.remove("is-visible");
      console.log("Removed is-visible class from container");
    }

    const signingStatus = document.getElementById("signingStatus");
    if (signingStatus) {
      signingStatus.classList.add("hidden");
      console.log("Added hidden class to signingStatus");
    }

    setTimeout(() => {
      if (this.container) {
        this.container.classList.add("is-hidden");
        console.log("Added is-hidden class to container");
      }
    }, 300);
  }

  updateUI() {
    console.log("updateUI called");
    if (!this.registry) {
      console.warn("Registry not set");
      return;
    }

    const activeSessions = this.registry.getActiveSessions();
    console.log("Active sessions:", activeSessions);
    const hasActiveSessions = activeSessions.length > 0;

    // Update the counter badge
    if (this.activeCountBadge) {
      this.activeCountBadge.textContent = activeSessions.length;
      this.activeCountBadge.classList.toggle("is-hidden", !hasActiveSessions);
    }

    // Only update container contents if visible
    if (this.visible && this.container) {
      console.log("UI is visible, updating container contents");
      this.container.innerHTML = "";

      if (!hasActiveSessions) {
        const emptyState = document.createElement("div");
        emptyState.className = "notification is-info is-light";
        emptyState.textContent = "No active signing sessions";
        this.container.appendChild(emptyState);
      } else {
        console.log("Creating session cards:", activeSessions.length);
        activeSessions.forEach((session, index) => {
          console.log(`Creating card ${index + 1}:`, session);
          try {
            const card = this.createSessionCard(session);
            if (card) {
              this.container.appendChild(card);
              console.log(`Card ${index + 1} added to container`);
            }
          } catch (error) {
            console.error(`Error creating card ${index + 1}:`, error);
          }
        });
      }
    }
  }

  createSessionCard(session) {
    console.log("Creating card for session:", session);
    const card = document.createElement("div");
    card.className = "box signing-session-card";
    card.style.marginBottom = "1rem";

    // Create card content
    const content = document.createElement("div");
    content.className = "content";

    // Add competition details
    const header = document.createElement("div");
    header.className = "mb-3";
    header.innerHTML = `
          <h4 class="title is-5 mb-2">Competition Signing</h4>
          <div class="tags">
              <span class="tag is-info">${session.state}</span>
          </div>
      `;

    // Add details
    const details = document.createElement("div");
    details.className = "mb-3";
    details.innerHTML = `
          <p><strong>Competition ID:</strong> <span class="has-text-info">${session.competitionId}</span></p>
          <p><strong>Entry ID:</strong> <span class="has-text-info">${session.entryId}</span></p>
          <p><strong>Status:</strong> <span class="has-text-primary">${this.getStatusMessage(session.state)}</span></p>
      `;

    // Add progress bar
    const progress = document.createElement("div");
    progress.className = "mt-4";
    progress.innerHTML = `
          <progress class="progress is-info"
                    value="${this.getProgressPercentage(session.state)}"
                    max="100">
              ${this.getProgressPercentage(session.state)}%
          </progress>
      `;

    // Assemble the card
    content.appendChild(header);
    content.appendChild(details);
    content.appendChild(progress);
    card.appendChild(content);

    console.log("Card created:", card);
    return card;
  }

  getCardStateClass(state) {
    switch (state) {
      case "COMPLETED":
        return "completed";
      case "ERROR":
        return "error";
      case "NONCES_SUBMITTED":
      case "AGGREGATE_NONCES_RECEIVED":
        return "needs-attention";
      default:
        return "";
    }
  }

  getStatusMessage(state) {
    switch (state) {
      case "INITIALIZED":
        return "Waiting for competition contract...";
      case "CONTRACT_RECEIVED":
        return "Contract received, generating nonces...";
      case "NONCES_SUBMITTED":
        return "Waiting for other participants...";
      case "AGGREGATE_NONCES_RECEIVED":
        return "Generating signatures...";
      case "COMPLETED":
        return "Signing completed!";
      case "ERROR":
        return "Error occurred during signing";
      default:
        return "Unknown state";
    }
  }

  getProgressPercentage(state) {
    const states = [
      "INITIALIZED",
      "CONTRACT_RECEIVED",
      "NONCES_SUBMITTED",
      "AGGREGATE_NONCES_RECEIVED",
      "COMPLETED",
    ];
    const index = states.indexOf(state);
    return index >= 0 ? (index / (states.length - 1)) * 100 : 0;
  }

  createProgressSteps(currentState) {
    const steps = [
      { state: "INITIALIZED", label: "1" },
      { state: "CONTRACT_RECEIVED", label: "2" },
      { state: "NONCES_SUBMITTED", label: "3" },
      { state: "AGGREGATE_NONCES_RECEIVED", label: "4" },
      { state: "COMPLETED", label: "✓" },
    ];

    return steps
      .map((step) => {
        const isCompleted = this.isStateCompleted(currentState, step.state);
        const isActive = currentState === step.state;
        return `
        <div class="progress-step ${isCompleted ? "completed" : ""} ${isActive ? "active" : ""}">
          ${step.label}
        </div>
      `;
      })
      .join("");
  }

  isStateCompleted(currentState, stepState) {
    const states = [
      "INITIALIZED",
      "CONTRACT_RECEIVED",
      "NONCES_SUBMITTED",
      "AGGREGATE_NONCES_RECEIVED",
      "COMPLETED",
    ];
    return states.indexOf(currentState) > states.indexOf(stepState);
  }

  isActiveState(state) {
    return state !== "COMPLETED" && state !== "ERROR";
  }

  verifyDOMStructure() {
    console.log("Verifying DOM structure:");
    console.log("Container:", this.container);
    console.log(
      "Signing status div:",
      document.getElementById("signingStatus"),
    );
    console.log("Active count badge:", this.activeCountBadge);
    console.log("Toggle button:", this.toggleButton);
  }

  cleanup() {
    console.log("Cleaning up signing UI");
    this.visible = false;
    this.activeSessionsCount = 0;

    if (this.container) {
      this.container.innerHTML = "";
      this.container.classList.add("is-hidden");
      this.container.classList.remove("is-visible");
    }

    const signingStatus = document.getElementById("signingStatus");
    if (signingStatus) {
      signingStatus.classList.add("hidden");
    }

    if (this.activeCountBadge) {
      this.activeCountBadge.textContent = "0";
      this.activeCountBadge.classList.add("is-hidden");
    }

    if (this.toggleButton) {
      this.toggleButton.classList.add("is-hidden");
    }
  }
}

export { SigningProgressUI };
