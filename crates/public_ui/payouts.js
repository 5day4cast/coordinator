export async function displayPayouts(apiBase, oracleBase) {
  const $tableBody = document.getElementById("payoutsTableBody");
  const $noPayouts = document.getElementById("noPayoutsMessage");
  const $error = document.getElementById("payoutsError");
  const $table = document.getElementById("usersPayouts");

  try {
    // Clear previous state
    $tableBody.innerHTML = "";
    $noPayouts.classList.add("hidden");
    $error.classList.add("hidden");

    // Fetch user's entries
    const response = await fetch(`${apiBase}/entries`, {
      headers: {
        "Content-Type": "application/json",
      },
    });

    if (!response.ok) {
      throw new Error(`Failed to fetch entries: ${response.status}`);
    }

    const entries = await response.json();
    entries.sort((a, b) => a.id.localeCompare(b.id));

    // Filter for payable entries
    const payableEntries = entries.filter(
      (entry) =>
        entry.signed_at && !entry.paid_out_at && !entry.payout_ln_invoice,
    );

    if (payableEntries.length === 0) {
      $table.classList.add("hidden");
      $noPayouts.classList.remove("hidden");
      return;
    }

    // Show table and populate it
    $table.classList.remove("hidden");
    payableEntries.forEach((entry) => {
      const $row = document.createElement("tr");
      $row.classList.add("is-clickable");
      $row.innerHTML = `
                    <td>${entry.event_id}</td>
                    <td>${entry.id}</td>
                    <td>Ready for payout</td>
                    <td>
                        <button class="button is-primary submit-payout"
                                data-competition-id="${entry.event_id}"
                                data-entry-id="${entry.id}"
                                data-entry-index="${entries.findIndex((e) => e.id === entry.id)}">
                            Submit Invoice
                        </button>
                    </td>
                `;
      $tableBody.appendChild($row);
    });

    // Add click handlers
    document.querySelectorAll(".submit-payout").forEach((button) => {
      button.addEventListener("click", () =>
        showPayoutModal(
          button.dataset.competitionId,
          button.dataset.entryId,
          parseInt(button.dataset.entryIndex),
          window.taprootWallet,
        ),
      );
    });
  } catch (error) {
    console.error("Error fetching payouts:", error);
    $table.classList.add("hidden");
    $error.textContent = `Error loading payouts: ${error.message}`;
    $error.classList.remove("hidden");
  }
}

async function showPayoutModal(competitionId, entryId, entryIndex, wallet) {
  const $modal = document.getElementById("payoutModal");
  const $submitButton = document.getElementById("submitPayoutInvoice");
  const $cancelButton = document.getElementById("cancelPayoutModal");
  const $closeButton = $modal.querySelector(".modal-close");
  const $error = document.getElementById("payoutModalError");
  const $invoice = document.getElementById("lightningInvoice");

  // Reset modal state
  $invoice.value = "";
  $error.classList.add("hidden");
  $modal.classList.add("is-active");

  const closeModal = () => {
    $modal.classList.remove("is-active");
  };

  // Setup event handlers
  $cancelButton.onclick = closeModal;
  $closeButton.onclick = closeModal;
  $modal.querySelector(".modal-background").onclick = closeModal;

  $submitButton.onclick = async () => {
    try {
      const invoice = $invoice.value.trim();

      if (!invoice) {
        throw new Error("Please enter a Lightning invoice");
      }

      const nostrPubkey = await window.nostrClient.getPublicKey();

      const payout_preimage = await wallet.getEncryptedDlcPayoutPreimage(
        entryIndex,
        nostrPubkey,
      );

      const ephemeral_private_key = await wallet.getEncryptedDlcPrivateKey(
        entryIndex,
        nostrPubkey,
      );

      // Submit payout request
      const payoutResponse = await fetch(
        `${apiBase}/competitions/${competitionId}/entries/${entryId}/payout`,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
          },
          body: JSON.stringify({
            ticket_id: entryId,
            payout_preimage,
            ephemeral_private_key,
            ln_invoice: invoice,
          }),
        },
      );

      if (!payoutResponse.ok) {
        throw new Error(`Failed to submit payout: ${payoutResponse.status}`);
      }

      closeModal();
      displayPayouts(apiBase, oracleBase);
    } catch (error) {
      console.error("Error submitting payout:", error);
      $error.textContent = error.message;
      $error.classList.remove("hidden");
    }
  };
}
