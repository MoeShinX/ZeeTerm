-- 主机协议：SSH（默认）/ Telnet / RDP，外加 X11 转发开关和各协议的选项。
--
-- 写好的迁移不再改，新增列走新迁移。老主机全部是 SSH、X11 关、没有选项，所以三列都有
-- 默认值；不给 protocol 建索引（没有按协议查的场景，且基线里不能给后续迁移才加的列建索引）。
-- protocol_options 是 JSON：{"telnet": {...}} 或 {"rdp": {...}}，SSH 为 NULL。
ALTER TABLE hosts ADD COLUMN protocol TEXT NOT NULL DEFAULT 'ssh';
ALTER TABLE hosts ADD COLUMN x11_forwarding INTEGER NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN protocol_options TEXT;
