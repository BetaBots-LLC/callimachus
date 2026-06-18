-- Cline-architecture VS Code agents that fork the same task storage layout.
--   roo  — Roo Code   <editor>/globalStorage/rooveterinaryinc.roo-cline/tasks/
--   kilo — Kilo Code  <editor>/globalStorage/kilocode.kilo-code/tasks/
INSERT OR IGNORE INTO sources (kind) VALUES ('roo'), ('kilo');
