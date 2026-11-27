-- Pro waitlist: emails collected from the landing page for the upcoming
-- hosted Pro tier. One row per email; the API returns the same 200 response
-- for new and duplicate emails, so the unique index only keeps the table
-- clean — it is never surfaced to the caller.
CREATE TABLE waitlist (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  email TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL
);
