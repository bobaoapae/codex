ALTER TABLE workflow_receipts
    ADD COLUMN references_json TEXT
    CHECK(references_json IS NULL OR length(references_json) <= 65536);
