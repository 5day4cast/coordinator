-- Keep provider-authenticated invoice receipts across refund retries and restarts.
-- Missing historical receipts must fail closed; they cannot be synthesized from metadata.
CREATE TABLE refund_invoice_authorizations (
    keygen_session_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    invoice TEXT NOT NULL,
    authorization TEXT NOT NULL,
    PRIMARY KEY (keygen_session_id, user_id, invoice)
);
