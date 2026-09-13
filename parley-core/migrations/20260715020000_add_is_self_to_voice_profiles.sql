-- Mark one voice profile as "this is the local user's own voice".
--
-- Populated by the "Record my voice" enrollment flow (see
-- `speaker_diarization::enrollment`): the user records a short baseline of
-- their own speech outside a meeting, we embed it, and store the centroid as
-- a normal voice profile with `is_self = 1`.
--
-- Why a flag rather than a naming convention: the enrolled profile's *name*
-- is what the diarizer renders on a transcript ("Me"), and names are
-- user-facing/mutable. The flag is the stable identity used by the
-- enrollment commands to find, replace, and delete the self profile.
--
-- The partial unique index enforces the "at most one self profile"
-- invariant in the schema itself, so a buggy caller gets a constraint
-- violation rather than silently creating a second "Me". Rows with
-- `is_self = 0` are not in the index (SQLite partial index), so ordinary
-- profiles are unaffected.

ALTER TABLE voice_profiles ADD COLUMN is_self INTEGER NOT NULL DEFAULT 0;

CREATE UNIQUE INDEX IF NOT EXISTS idx_voice_profiles_single_self
    ON voice_profiles(is_self) WHERE is_self = 1;
