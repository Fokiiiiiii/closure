-- Version 2 envelopes carry a key ID byte, so the largest stored value grows by one byte.
-- Existing rows are version 1 and stay valid: the lower bound and every other column are unchanged.
ALTER TABLE integration_secrets
    DROP CONSTRAINT integration_secrets_encrypted_value_check,
    ADD CONSTRAINT integration_secrets_encrypted_value_check
        CHECK (octet_length(encrypted_value) BETWEEN 29 AND 16414);
