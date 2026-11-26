# Conventions

## Guards (added after a mistake repeated)
- Before calling UI work done, render it in the browser at 390px and 1280px and check overflow, clipped text, overlap and contrast.
- Before finishing, stop the servers and background commands you started; long-lived work goes in a job or the preview.

## How UI changes are served
- static/*.html files are embedded into the binary via include_str! (src/ui.rs). After editing them, `cargo build` and restart the aghub jobs — reloading the browser is not enough.
- The app UI runs as jobs: aghub-verify on PORT=3000 (AGHUB_DB=/tmp/aghub-ui-verify.db) and aghub-server on 3001. The browser tool reaches the app at the public preview host on port 3000.
- Composer sends on form submit (Send button); Enter inside the composer textarea inserts a newline. In browser tests, click the submit button.
- Signup/login forms use plain ids (#name, #handle, #email, #password, #submit); the signup click auto-navigates to /app.
