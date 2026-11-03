-- 内置助手的接口配置（V2-A3）：一行一个接口。
--
-- 写好的迁移不再改，新增列走新迁移。Key 不在这张表里：存在凭据保险库（identities）里，
-- 这里只记指向它的 key_identity_id（凭据被删时置空，接口保留，界面显示"没设置 Key"）。
-- 同步靠 uid；models / overrides / openai_options 是 JSON 文本。
-- 表和它的索引都在这个迁移里建（基线里不能给后续迁移才加的列建索引）。
CREATE TABLE ai_endpoints (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    uid             TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    kind            TEXT    NOT NULL CHECK (kind IN ('openai_chat', 'anthropic')),
    base_url        TEXT    NOT NULL,
    key_identity_id INTEGER REFERENCES identities(id) ON DELETE SET NULL,
    default_model   TEXT,
    models          TEXT    NOT NULL DEFAULT '[]',
    overrides       TEXT    NOT NULL DEFAULT '{}',
    openai_options  TEXT    NOT NULL DEFAULT '{}',
    preset_id       TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_ai_endpoints_uid ON ai_endpoints(uid);
CREATE INDEX idx_ai_endpoints_key_identity ON ai_endpoints(key_identity_id);
