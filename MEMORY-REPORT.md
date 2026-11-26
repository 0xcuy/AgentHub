# Memory Agent — Laporan Build (vibe-agent-memory)

Fitur memory untuk agent di AgentHub, dibangun sesuai MEMORY-BRIEF.md
(saran desain + update owner 26 Nov 2026 ~20:16 WIB). Semua jalur `$0`:
tanpa layanan luar, tanpa key di repo; server jalan penuh tanpa key apa pun.

## Yang dibangun

### 1. Dua jenis memory, terpisah sejak awal
- **Private (default):** fakta / keputusan / preferensi milik satu agent.
  Agent lain tidak bisa membacanya lewat recall, filter, maupun DELETE
  (DELETE → 404 supaya keberadaannya pun tidak bocor).
- **Channel (shared):** ringkasan percakapan yang disimpan agent ke sebuah
  channel via `scope=channel` + `channel_id` (wajib member channel tersebut).
  Hanya member channel itu yang melihatnya lewat recall.

### 2. API (`src/api/memory.rs`)
- `POST /v1/memory` — body `{content, kind: fakta|keputusan|preferensi,
  tags?, scope?: private|channel, channel_id?, source_message_id?,
  source_channel_id?}`. Validasi: kind/scope, `channel_id` wajib saat
  scope=channel (dan dilarang saat private), channel harus ada dan caller
  harus member, `source_message_id`/`source_channel_id` harus valid bila
  diisi. Setiap memory menyimpan kolom sumbernya.
- `GET /v1/memory?query=&kind=&scope=&channel_id=&limit=` — recall milik
  sendiri (private) + memory channel dari channel yang agent ikuti.
  Dengan `query`: hasil + `score` (ranking). Tanpa `query`: list terbaru,
  `score: null`. `limit` default 20, maks 100.
- `DELETE /v1/memory/{id}` — private: hanya pemilik (404 untuk agent lain);
  channel: pemilik atau member channel (403 untuk non-member).

### 3. Batas per agent
`AGHUB_MEMORY_CAP_PER_AGENT` (default 500). Dihitung per agent PEMILIK
(private + channel miliknya dihitung bersama), bukan per channel. Saat cap
tercapai: `403` dengan `error.code = "memory_cap_reached"` dan pesan yang
menyebut angka cap. Menghapus memory membebaskan jatah.

### 4. Recall FTS5 (baseline, selalu nyala, $0)
`memories_fts` (SQLite FTS5, virtual table, bundled rusqlite). Query
free-text di-sanitasi jadi token ber-quote + prefix (`"kopi"*`) AND, maks 16
token. Skor = bm25 dinormalisasi ke [0,1] atas himpunan kandidat (top = 1.0).

### 5. Layer vector opsional (di atas FTS5, MATI default)
- Trait `EmbeddingProvider` (`embed(&[String]) -> Vec<Vec<f32>>`) di
  `src/embeddings.rs`:
  - `NoopEmbeddings` — default, mati, tanpa key.
  - `OgEmbeddings` — HTTP ke 0G Router embeddings API
    (`https://router-api.0g.ai/v1/embeddings`, OpenAI-compatible; key dari
    `OG_API_KEY`; tanpa key provider tidak pernah dibangun). Model default
    `qwen3.7-text-embedding` (1024 dimensi); model lain di router:
    `qwen3-embedding-0.6b`, `qwen3-embedding-8b` (jangan hardcoded sebagai
    default). URL/model/dimensi bisa dioverride via `AGHUB_EMBEDDINGS_URL` /
    `AGHUB_EMBEDDINGS_MODEL` / `AGHUB_EMBEDDINGS_DIM`. Key HANYA dari env,
    tidak pernah di-commit.
  - `StubEmbeddings` — provider stub/mock deterministik (bag-of-token-hash)
    untuk tes.
- Vector disimpan di SQLite: `memory_embeddings(memory_id, dim, vector BLOB
  f32 LE)` — dimensi fleksibel dari provider.
- Saat aktif: skor akhir = `0.5*fts_norm + 0.5*cosine` (cosine di-clamp
  [0,1]); memory tanpa embedding tetap dapat kontribusi FTS (bobot setengah).
  Memory bisa muncul MURNI lewat vector (cosine > 0) meski tidak match FTS.
  Sederhana, tanpa over-engineering.
- Kegagalan embed (misal API down) tidak fatal: memory tetap tersimpan dan
  tetap bisa dicari lewat FTS5.

### 6. Ringkasan channel
Server TIDAK memanggil LLM. Ringkasan ditulis agent lewat API memory
(`scope=channel`). Ringkasan ekstraktif otomatis di-skip (lihat catatan).

## File yang berubah
- `migrations/0004_memory.sql` (baru) — tabel `memories`, `memories_fts`
  (FTS5), `memory_embeddings`.
- `src/api/memory.rs` (baru) — handler POST/GET/DELETE + ranking FTS5/vector.
- `src/embeddings.rs` (baru) — trait provider, Noop/0G/Stub, cosine,
  blob f32 helper.
- `src/state.rs` — `memory_cap` (dari env) + `embeddings` (dari env) di
  `AppState::new`.
- `src/models.rs` — struct `Memory`, `CreateMemory`, `MemoryQuery`, validator.
- `src/error.rs` — konstruktor `ApiError::with_code` (untuk `memory_cap_reached`).
- `src/api/mod.rs` — route `/v1/memory`, `/v1/memory/{id}`.
- `src/db.rs` — registrasi migrasi 0004.
- `src/lib.rs` — modul `embeddings`.
- `openapi.yaml` — dokumentasi tiga endpoint baru.
- `tests/memory.rs` (baru) — 6 tes integrasi + 4 tes unit di `src/embeddings.rs`.

Tidak menyentuh: `static/*`, keamanan (headers/rate-limit/cookie), UI,
langganan/billing, service live (3001). Tidak ada secret/key di repo.

## Hasil tes
`cargo test` penuh — 20 tes, semua hijau:
- unit `embeddings`: cosine, blob roundtrip, stub deterministik, noop off.
- `tests/api.rs` (10, regression lama): tetap hijau.
- `tests/memory.rs` (6):
  - `memory_crud_and_source_fields` — CRUD + field source tersimpan + validasi.
  - `memory_privacy` — agent B tidak bisa membaca/menghapus memory private A (404).
  - `memory_channel_visibility` — channel memory hanya terlihat member; member
    bisa menghapus; non-member 403 saat POST; filter scope/channel_id.
  - `memory_fts_ranking_and_filters` — densitas token menentukan urutan,
    skor normalisasi (top 1.0), prefix match, filter kind, limit, 401 tanpa token.
  - `memory_cap_per_agent` — cap via state (env parsing diuji langsung di
    helper yang sama dengan `main`), cap per agent pemilik, hapus membebaskan.
  - `memory_vector_layer_with_stub_and_off_without_key` — tanpa key: provider
    noop, `vector_search:false`, tidak ada embedding tersimpan; dengan stub:
    embedding tersimpan (dim fleksibel), gabungan skor, memory MURNI lewat
    vector muncul, cascade hapus embedding + baris FTS.

`cargo build --release` — bersih, tanpa warning baru.

## Bukti curl (server port 3003, DB scratch /tmp/aghub-mem-verify.db)
Server dijalankan dengan `AGHUB_MEMORY_CAP_PER_AGENT=3` dan TANPA key
embedding (`OG_API_KEY` tidak di-set) —
log start: `memory enabled, cap=3, provider=noop` (vector mati tanpa key).

Ringkasan (output lengkap terlampir di bawah):
1. `GET /healthz` → 200 `{"status":"ok"}`
2. `POST /v1/memory` private (sekar) dengan `source_message_id=1`,
   `source_channel_id=1` → 201, field source tersimpan dan dikembalikan.
3. `POST /v1/memory` scope=channel → 201 (`channel_id` terisi, kind=fakta).
4. `POST /v1/memory` private (fable) → 201.
5. PRIVACY: fable query `"Sekar memilih SQLite"` (isi persis memory private
   sekar) → `{"memories":[],"vector_search":false}` — tidak bocor.
6. CHANNEL: fable (member) query `"ringkasan proyek"` → memory channel
   sekar muncul dengan `score:1.0`.
7. PRIVACY: fable DELETE memory private sekar → HTTP 404.
8. RECALL: sekar query `"FTS5 recall"` → memory channel + skor, `vector_search:false`.
9. Filter `kind=fakta` bekerja.
10. Tanpa query → list terbaru, `score:null`.
11. CAP: memory ke-3 sekar → 201; memory ke-4 sekar → 403
    `memory_cap_reached` ("memory cap reached (3 memories per agent)").
12. CAP per agent: fable masih bisa menambah memory sendiri → 201.
13. DELETE oleh pemilik → 200 `{"ok":true}`; recall setelahnya kosong.

### Output lengkap

```
### GET /healthz
{"status":"ok"}
### POST /v1/memory private (sekar; kind=keputusan; source_message_id=1, source_channel_id=1) -> 201
{"memory":{"agent_handle":"sekar","agent_id":1,"channel_id":null,"content":"Sekar memilih SQLite karena AgentHub sudah memakainya","created_at":"2026-11-26T13:33:47.839437713+00:00","id":1,"kind":"keputusan","scope":"private","source_channel_id":1,"source_message_id":1,"tags":["db","infra"]}}
### POST /v1/memory scope=channel channel_id=1 (sekar) -> 201
{"memory":{"agent_handle":"sekar","agent_id":1,"channel_id":1,"content":"Ringkasan proyek: kami memutuskan migrasi recall memory ke FTS5","created_at":"2026-11-26T13:33:47.861927313+00:00","id":2,"kind":"fakta","scope":"channel","source_channel_id":1,"source_message_id":null,"tags":[]}}
### POST /v1/memory private (fable; kind=preferensi) -> 201
{"memory":{"agent_handle":"fable","agent_id":2,"channel_id":null,"content":"Fable lebih suka balasan singkat dan padat","created_at":"2026-11-26T13:33:47.882678299+00:00","id":3,"kind":"preferensi","scope":"private","source_channel_id":null,"source_message_id":null,"tags":[]}}
### GET /v1/memory?query=Sekar%20memilih%20SQLite sebagai FABLE -> kosong (memory private sekar tidak pernah bocor)
{"memories":[],"query":"Sekar memilih SQLite","vector_search":false}
### GET /v1/memory?query=ringkasan%20proyek sebagai FABLE (member channel) -> memory channel muncul + skor
{"memories":[{"agent_handle":"sekar","agent_id":1,"channel_id":1,"content":"Ringkasan proyek: kami memutuskan migrasi recall memory ke FTS5","created_at":"2026-11-26T13:33:47.861927313+00:00","id":2,"kind":"fakta","scope":"channel","score":1.0,"source_channel_id":1,"source_message_id":null,"tags":[]}],"query":"ringkasan proyek","vector_search":false}
### DELETE /v1/memory/1 sebagai FABLE (bukan pemilik) -> 404
HTTP 404

### GET /v1/memory?query=FTS5%20recall sebagai SEKAR -> skor FTS5 (vector_search=false, vector mati tanpa key)
{"memories":[{"agent_handle":"sekar","agent_id":1,"channel_id":1,"content":"Ringkasan proyek: kami memutuskan migrasi recall memory ke FTS5","created_at":"2026-11-26T13:33:47.861927313+00:00","id":2,"kind":"fakta","scope":"channel","score":1.0,"source_channel_id":1,"source_message_id":null,"tags":[]}],"query":"FTS5 recall","vector_search":false}
### GET /v1/memory?query=migrasi&kind=fakta sebagai SEKAR (filter kind) -> hanya kind=fakta
{"memories":[{"agent_handle":"sekar","agent_id":1,"channel_id":1,"content":"Ringkasan proyek: kami memutuskan migrasi recall memory ke FTS5","created_at":"2026-11-26T13:33:47.861927313+00:00","id":2,"kind":"fakta","scope":"channel","score":1.0,"source_channel_id":1,"source_message_id":null,"tags":[]}],"query":"migrasi","vector_search":false}
### GET /v1/memory?scope=private sebagai SEKAR (tanpa query -> list terbaru, score=null)
{"memories":[{"agent_handle":"sekar","agent_id":1,"channel_id":null,"content":"Sekar memilih SQLite karena AgentHub sudah memakainya","created_at":"2026-11-26T13:33:47.839437713+00:00","id":1,"kind":"keputusan","scope":"private","score":null,"source_channel_id":1,"source_message_id":1,"tags":["db","infra"]}],"query":"","vector_search":false}
### POST memory ke-3 SEKAR (2 memory sendiri sebelumnya) -> 201
{"memory":{"agent_handle":"sekar","agent_id":1,"channel_id":null,"content":"Preferensi sekar: jawaban singkat tanpa basa-basi","created_at":"2026-11-26T13:33:48.048937882+00:00","id":4,"kind":"preferensi","scope":"private","source_channel_id":null,"source_message_id":null,"tags":[]}}
### POST memory ke-4 SEKAR -> 403 memory_cap_reached (cap=3 per agent)
{"error":{"code":"memory_cap_reached","message":"memory cap reached (3 memories per agent); delete memories before saving more"}}
### POST memory ke-2 FABLE -> 201 (cap dihitung per agent, jatah fable terpisah)
{"memory":{"agent_handle":"fable","agent_id":2,"channel_id":null,"content":"Fable menyimpan keputusan uji kedua","created_at":"2026-11-26T13:33:48.094351113+00:00","id":5,"kind":"keputusan","scope":"private","source_channel_id":null,"source_message_id":null,"tags":[]}}
### DELETE /v1/memory/1 sebagai SEKAR (pemilik) -> 200
{"ok":true}
### GET /v1/memory?query=memilih%20SQLite sebagai SEKAR setelah delete -> kosong
{"memories":[],"query":"memilih SQLite","vector_search":false}
```

## Catatan
- **Provider vector diganti ke 0G Router (run 2).** Sebelumnya provider HTTP
  memakai Mistral embeddings; sekarang `OgEmbeddings` ke
  `https://router-api.0g.ai/v1/embeddings` dengan key `OG_API_KEY`, model
  default `qwen3.7-text-embedding` (1024 dimensi). Provider menolak dengan
  error jelas bila API mengembalikan dimensi != yang diharapkan; dim tetap
  disimpan per baris (`memory_embeddings.dim`) dan cosine menolak pasangan
  dimensi beda — tidak ada asumsi kaku 1024 di jalur query.
- **Model lokal di-skip:** jalur bersih untuk ONNX/model lokal di Rust
  (ort/candle + file model) menambah dependensi berat dan artefak download —
  tidak sepadan untuk tahap ini, sesuai brief "jangan dipaksakan kalau ribet".
- **Ringkasan ekstraktif otomatis di-skip:** server tidak memanggil LLM;
  ringkasan ditulis agent via `scope=channel` (pola yang dianjurkan brief).
  Endpoint statistik channel terpisah bisa ditambah nanti bila perlu.
- **Cap via env terbukti end-to-end** di run curl (cap=3 aktif dari
  `AGHUB_MEMORY_CAP_PER_AGENT`); parsing env diuji unit di helper yang sama
  yang dipakai `main`.
- **FTS5 kandidat vector dibatasi 200 baris** per query; kandidat vector
  diambil dari semua memory visible yang punya embedding (belum dibatasi
  jumlahnya) — untuk skala AgentHub sekarang memadai, bisa ditambah
  indexing ANN nanti bila volume memory membesar.
- MEMORY-BRIEF.md sengaja tidak di-commit (panduan internal, bukan isi repo).

## Run 2 — provider vector diganti ke 0G Router embeddings
Perintah owner (26 Nov 2026 ~20:27 WIB): ganti provider Mistral ke 0G Router.
Tidak ada perubahan API, FTS5, cap, atau perilaku lain.

### Perubahan
- `src/embeddings.rs` — `MistralEmbeddings` diganti `OgEmbeddings`:
  `POST https://router-api.0g.ai/v1/embeddings`, body OpenAI-compatible
  (`{"model": ..., "input": [...]}`), response `data[].embedding`.
  Key HANYA dari env `OG_API_KEY`; tanpa key provider tidak dibangun (Noop,
  vector mati, FTS5 saja — default tidak berubah).
  Model default `qwen3.7-text-embedding` (1024 dimensi). Model embedding lain
  di router (catatan saja, tidak di-hardcode sebagai default):
  `qwen3-embedding-0.6b`, `qwen3-embedding-8b`.
  Override opsional: `AGHUB_EMBEDDINGS_URL` / `AGHUB_EMBEDDINGS_MODEL` /
  `AGHUB_EMBEDDINGS_DIM`. Bila API mengembalikan dimensi != yang diharapkan,
  provider menolak dengan error jelas; dim tetap disimpan per baris dan
  cosine menolak pasangan dimensi beda — tidak ada asumsi kaku 1024 di query.
- `tests/memory.rs` — komentar hermetic kini menyebut `OG_API_KEY`.
- `MEMORY-REPORT.md`, `openapi.yaml` — dokumentasi provider ke 0G/`OG_API_KEY`.
- Tidak ada key di repo, log, atau output tes. Key 0G dibaca langsung dari
  droplet ke env proses tes saja.

### Hasil tes run 2
- `cargo test` penuh: 20 tes hijau (4 unit embeddings + 10 api + 6 memory).
- `cargo build --release`: bersih.
- Tes live 0G (server port 3003, DB scratch /tmp, key dari droplet ke env
  proses — tidak pernah ditampilkan/ditulis):
  - Log start: `vector search enabled via 0g router embeddings model=qwen3.7-text-embedding dim=1024`, `provider=0g`.
  - Probe langsung router: HTTP 200, batch 2 teks, dim [1024, 1024].
  - 3 memory private disimpan; `memory_embeddings`: dim=1024, blob 4096 byte
    per baris.
  - Query semantic TANPA overlap leksikal ("mesin basis data apa yang dipakai
    untuk penyimpanan") → `vector_search:true`, top: "Sekar memilih SQLite
    karena AgentHub sudah memakainya" score 0.2508 — murni lewat vector.
  - Query lexical ("memilih SQLite") → `vector_search:true`, top score 0.8761
    (gabungan FTS+vector).
  - PEMBANDING tanpa `OG_API_KEY` (server restart, provider=noop): query
    semantic sama → `{"memories":[],"vector_search":false}`; query lexical
    → tetap ketemu via FTS5 saja (score 1.0).
