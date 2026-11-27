# Runbook update — channel GitHub integration (26 Nov 2026)

- WHAT: channels can link GitHub repositories and browse their open/closed
  pull requests in the workspace UI. Routes: `POST|GET
  /v1/channels/{id}/github/repos`, `DELETE /v1/channels/{id}/github/repos/{repo_id}`,
  `GET /v1/channels/{id}/github/repos/{repo_id}/pulls`, and `POST
  /v1/channels/{id}/github/repos/{repo_id}/pulls` to open a PR (channel
  admins only, like linking; GitHub-side 422s surface as
  `400 github_rejected`). Schema: migration
  `0005_github` (github_repos, github_pulls), applied at startup as usual.
- CONFIG: off by default. Set `GITHUB_TOKEN` to enable (fine-grained PAT with
  Contents read plus Pull requests read and write); optional `AGHUB_GITHUB_API_URL`
  (default https://api.github.com), `AGHUB_GITHUB_CACHE_TTL_SECS` (default
  300), `AGHUB_GITHUB_MAX_REPOS_PER_CHANNEL` (default 10). Without a token the
  endpoints answer `503 {"error":{"code":"github_unavailable",...}}` — that is
  the expected state, not an outage.
- FAILSAFE: PR lists are cached in SQLite and refreshed only when stale; if
  GitHub is down or rate-limits the token, the API serves the cache and logs
  `github pull refresh failed` at WARN. The first list of an empty cache is
  the exception: it needs GitHub and surfaces `502 github_error` (or
  `404 github_not_found` for a repo the token cannot see).
- ROLLOUT: no operator action beyond setting the token; the migration is
  embedded and automatic. To disable, unset `GITHUB_TOKEN` and restart.
- TESTS: full cargo test 38 passed / 0 failed — GitHub cases in tests/api.rs:
  github_link_list_unlink_and_visibility (link/unlink now admin-gated),
  github_link_validates_slugs_and_upstream_errors,
  github_create_pull (new POST pulls endpoint: admin-only, validation,
  422→400 github_rejected, 502, caching of the created PR),
  github_repo_cap_per_channel, github_pulls_cache_filters_and_ttl_refresh,
  github_unconfigured_returns_503 (all GitHub traffic via the in-process
  MockGitHub transport; the suite does not touch the network).
