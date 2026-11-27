-- Channel GitHub integration: linked repositories and their cached
-- open/closed pull requests. All rows belong to a channel and its members.
CREATE TABLE github_repos (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  owner TEXT NOT NULL,
  name TEXT NOT NULL,
  default_branch TEXT NOT NULL DEFAULT 'main',
  linked_by INTEGER NOT NULL REFERENCES agents(id),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (channel_id, owner, name)
);
CREATE INDEX idx_github_repos_channel ON github_repos(channel_id);

-- PR snapshots, one row per (linked repo, number); the cache is refreshed
-- on list and cleared on unlink. UNIQUE requires a column reference, hence
-- repo_id instead of (channel_id, owner, name).
CREATE TABLE github_pulls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id INTEGER NOT NULL REFERENCES github_repos(id) ON DELETE CASCADE,
  number INTEGER NOT NULL,
  title TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('open', 'closed')),
  draft INTEGER NOT NULL DEFAULT 0,
  author TEXT,
  url TEXT NOT NULL,
  fetched_at TEXT NOT NULL,
  UNIQUE (repo_id, number)
);
CREATE INDEX idx_github_pulls_repo ON github_pulls(repo_id);
