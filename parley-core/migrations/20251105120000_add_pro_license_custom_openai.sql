-- Migration: Add PRO License and Custom OpenAI Configuration (RSA-based)

-- This column stores: {endpoint, apiKey, model, maxTokens, temperature, topP}
ALTER TABLE settings ADD COLUMN customOpenAIConfig TEXT;

-- Drop and recreate licensing table with RSA structure
DROP TABLE IF EXISTS licensing;

CREATE TABLE licensing (
    license_key TEXT PRIMARY KEY,           -- Decrypted license ID
    encrypted_key TEXT NOT NULL,            -- Original encrypted key (RSA + Base64)
    signature_hash TEXT NOT NULL,           -- SHA-256 hash of encrypted_key for integrity
    activation_date TEXT NOT NULL,          -- ISO 8601 timestamp of activation
    expiry_date TEXT NOT NULL,              -- activation_date + duration
    soft_expiry_date TEXT NOT NULL,         -- expiry_date + grace period
    max_activation_time TEXT NOT NULL,      -- From decrypted license data
    duration INTEGER NOT NULL,              -- Duration in seconds
    generated_on TEXT NOT NULL,             -- ISO 8601 timestamp when license was generated
    is_soft_expired INTEGER DEFAULT 0       -- 0=active, 1=soft expired, 2=hard blocked
);
