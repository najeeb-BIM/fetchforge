# FetchForge — GitHub Guide for a Complete Beginner

This guide is for one job only: get FetchForge built on Windows by GitHub,
then install it on your own Windows computer and test it.

You do not need to learn Git, code, or the command line for this guide.
The only command in this guide is one copy-and-paste line in PowerShell
to check a file's fingerprint (its SHA-256 number). Everything else is
clicks in your browser and in Windows.

**Important: this build is a test build, not a finished product.**
The app has never run on Windows yet. That is exactly what this build
is for: to find out if it compiles and runs on Windows, and if the
browser extension pairs with it. Keep your expectations at that level.

**Never share your GitHub password with anyone, including me.**
I will never ask for it. If anyone asks for it, do not give it.

---

## Before you start — what you need

- Your Windows computer, with your files.
- The FetchForge source package zip (the clean zip from Step 5).
  Its SHA-256 fingerprint is written in the final report that came
  with this guide. Keep that number open while you do Step 3.
- A GitHub account. You already have one, so you do not need to
  create a new account.
- About 1 hour of waiting time for the first build. The build itself
  runs on GitHub's computer, not yours, so you can leave the page
  open and do other things. Do not keep clicking Run or Refresh
  again and again while you wait.

## A warning about time and GitHub minutes — please read this first

**The first Windows build can take a long time.**
One step in the build installs the Tauri tool with this command:
`cargo install tauri-cli --locked`. That command compiles Tauri
from source, which is slow. A first build of 30 to 60 minutes on
the screen is normal. It is not stuck just because it is slow.
Wait for the green tick or the red cross. Do not close the tab
in a panic, and do not start a second build.

**Windows builds use GitHub minutes at double speed on a private
repository.**
GitHub gives a free account 2,000 included minutes per month for
private repositories. Windows jobs count at 2 times the normal
rate. That means:

- 1 minute on the screen = 2 minutes from your free allowance.
- A 60-minute Windows build on the screen = 120 minutes from
  your allowance.
- 1,000 minutes of Windows builds on the screen would use up
  the whole 2,000-minute free allowance for the month.

Minutes reset each month and do not carry over. After the free
minutes are used, GitHub may charge money, or stop the job,
depending on your billing settings. Check your own billing page
if you are unsure, because GitHub changes its prices from time
to time. The 2,000-minute number and the 2-times Windows number
are the numbers GitHub publishes for the free plan.

**Rule for this build: run the workflow at most once, then stop.**
If the first run shows a red cross, do NOT click Run workflow
again. Copy the last 50 lines of the failed step's log (see
Step 7) and bring them to Claude. Guessing and re-running again
and again wastes minutes and does not fix the problem.

---

## Step 1 — Sign in to GitHub and check two-step login

1. Open your browser and go to github.com.
2. If you see a **Sign in** button at the top right, click it
   and sign in with your GitHub account.
3. After you sign in, you should SEE your profile picture (a
   small round picture) at the top right of the page. That means
   you are signed in.

**Check two-step login (this protects your account):**

4. Click your profile picture at the top right, then click
   **Settings**.
5. On the left side, click **Password and authentication**.
6. Look for the section called **Two-factor authentication**.
   - If it says two-factor authentication is on, or you see a
     button that says **Disable** or **Reconfigure**, it is
     already on. You are done with this step. Go to Step 2.
   - If you see a button that says **Enable two-factor
     authentication**, click it and follow the steps on the
     screen. GitHub will ask you to use an app on your phone,
     or a text message, or a security key. Follow the choice
     GitHub gives you, and save the recovery codes GitHub
     shows you in a safe place (not in this repository).
7. After this, you should SEE that two-factor authentication
   is on.

**If it looks different:** GitHub changes its pages from time
to time. If you cannot find **Password and authentication**,
click your profile picture, click **Settings**, and use the
search box at the top of the Settings page to search for
"two-factor". If you still cannot find it, stop and ask me
before you continue. Do not turn off any security setting.

---

## Step 2 — Create a new repository called fetchforge, Private

1. Click your profile picture at the top right, then click
   **Your repositories**.
2. Click the green **New** button.
3. You should SEE a page called **Create a new repository**.
4. In the box called **Repository name**, type exactly:

   `fetchforge`

5. Leave the description box empty. You do not need it.
6. Under the name, you will see two choices: **Public** and
   **Private**. Click **Private**.
   - You should SEE a short text under Private that says
     only people you choose can see it. That is what you want.
7. **Do NOT tick the box that says "Add a README file".**
   Leave it empty. If you tick it, GitHub will add a file
   that is not in the FetchForge package, and that only
   causes confusion.
8. Scroll down and click the green **Create repository**
   button at the bottom.
9. You should SEE an empty repository page with the name
   **fetchforge** at the top, and a message that says
   something like "Get started by creating a new file or
   uploading an existing file". That means the repository
   is ready.

**If it looks different:** if GitHub says a repository called
fetchforge already exists, do not create a second one with a
different name. Stop and tell me, so we can check the old one
first.

---

## Step 3 — Unzip the package and check its fingerprint

Do this on your Windows computer, before you upload anything.

1. Find the FetchForge source package zip file you received.
   It is a `.zip` file.
2. Right-click the zip file, then click **Extract All**.
3. In the small window that appears, click **Extract**.
4. You should SEE a new folder with the same name as the zip
   file (without `.zip` at the end). Open that folder.
5. Inside, you should SEE files and folders. At the top level
   you should see folders like `apps`, `crates`, `extensions`,
   `installer`, `scripts`, and `tests`, and files like
   `README.md` and `GITHUB_GUIDE.md` (this guide).
   If you only see one folder inside, open that folder — the
   real files are inside it.

**Check the fingerprint (SHA-256) of the zip file:**

6. In Windows, open **PowerShell**. To do that, click the
   Start button, type `PowerShell`, and click **Windows
   PowerShell**.
7. In PowerShell, type this command, but replace
   `<zipname>` with the real name of your zip file, and
   replace the folder part with where the file actually is.
   For example, if the file is on your Desktop, the command
   looks like this:

   ```powershell
   Get-FileHash .\fetchforge-source.zip -Algorithm SHA256
   ```

   If your zip file has a different name, use that name
   instead of `fetchforge-source.zip`. Do not type the
   `<` and `>` signs.
8. Press Enter. You should SEE a line with a long number
   made of letters and numbers, under the word **Hash**.
9. Compare that number, letter by letter, with the SHA-256
   number in the final report that came with this guide.
   - If the numbers match exactly, the zip file is good.
     Go to Step 4.
   - If the numbers do NOT match, do NOT upload the files.
     Stop and tell me. The zip file may be damaged or
     changed, and uploading it would waste a build.

**If it looks different:** if PowerShell says the file was
not found, check the file name and the folder. The easiest
way is to open the folder that has the zip file, right-click
inside the folder, and open PowerShell from there, then use
just the file name in the command.

---

## Step 4 — Upload the files INSIDE the unzipped folder

**Upload the files and folders, not the zip file itself.**
If you upload the zip file, GitHub will not run the build,
because the build needs to see the files, not a zip file.

1. Go back to your new **fetchforge** repository page on
   GitHub (see Step 2).
2. Click **Add file**, then click **Upload files**.
3. You should SEE a page that says "Drag files here to add
   them to your repository".
4. Open the unzipped folder on your computer (from Step 3).
   Select ALL the files and folders INSIDE it, and drag
   them into the browser page. Do not drag the zip file,
   and do not drag the outer folder itself — drag what is
   inside it.
5. Wait. You should SEE a list of the uploaded files appear
   on the page. This can take a minute or two.
6. Scroll down. You should SEE a green **Commit changes**
   button. Click it.
7. Wait again. You should SEE the repository page again,
   now with files and folders listed, instead of the empty
   message from Step 2.

**Never upload these things, even if you see them on your
computer:**

- A folder called `target`
- A folder called `backups`
- Any file with `token` in its name
- Any file with `.key` or `.pem` in its name
- Any file with `.env` in its name
- Any file that ends with `.log`

The clean package zip does not contain any of these, so if
you upload only what is inside the unzipped folder, you
should be safe. If you are ever unsure about a file, do not
upload it, and ask me first.

**If the browser upload limit is a problem (about 100 files
at once): use GitHub Desktop instead.**

GitHub Desktop is a free program from GitHub that uploads
files from your computer without the browser limit. Use it
only if the browser upload fails or says there are too many
files:

1. Go to desktop.github.com and download GitHub Desktop.
   Install it on your Windows computer.
2. Open GitHub Desktop and sign in with your GitHub account.
3. In GitHub Desktop, click **File**, then **Clone
   repository**, then find **fetchforge** in the list, and
   click **Clone**. Choose a folder on your computer for it.
4. Copy ALL the files and folders from inside the unzipped
   FetchForge folder into the folder GitHub Desktop just
   made. Do not copy the zip file, and do not copy `target`,
   `backups`, or any token or key file (see the list above).
5. In GitHub Desktop, you should SEE the changed files in
   the left panel. At the bottom left, type a short message
   like "FetchForge source", then click **Commit to main**.
6. At the top, click **Push origin**. Wait until it says
   the push is done.
7. Go back to the repository page on github.com. You should
   SEE the files there, just like in the browser upload.

---

## Step 5 — Check that the workflow file is really there

Folders that start with a dot, like `.github`, sometimes do
not upload correctly in a browser. The build cannot run
without the workflow file, so check it now.

1. On your repository page, look at the list of files and
   folders. Look for a folder called **.github**.
2. Click **.github**, then click **workflows**, then click
   **windows-build.yml**.
3. You should SEE a file that starts with these lines:

   ```yaml
   name: Windows build
   ```

   and has steps called **Install NSIS**, **Install Tauri
   CLI**, **Run tests**, and **Build Tauri app, NSIS
   installer and portable zip**. That means the workflow
   file uploaded correctly. Go to Step 6.

**If the .github folder or the file is missing, add it by hand:**

4. On the repository page, click **Add file**, then click
   **Create new file**.
5. In the box at the top where it says "Name your file…",
   type exactly:

   `.github/workflows/windows-build.yml`

   When you type the `/` signs, GitHub will make new
   folders for you. That is normal.
6. Open the file called `windows-build.yml` from inside
   your unzipped FetchForge folder, on your computer, in
   Notepad. Select all the text in it (Ctrl + A), copy it
   (Ctrl + C), then paste it (Ctrl + V) into the big text
   box on GitHub.
7. Scroll down and click the green **Commit changes**
   button.
8. Go back and check again: click **.github**, then
   **workflows**, then **windows-build.yml**. You should
   SEE the file now.

**If it looks different:** if GitHub does not let you type
a file name with a dot at the start, or if the paste does
not work, stop and tell me. Do not type the workflow from
memory — a small typing mistake will break the build.

---

## Step 6 — Run the Windows build (at most once)

1. On your repository page, click the **Actions** tab at
   the top of the page.
2. If GitHub shows a button that says something like
   "I understand my workflows, go ahead and enable them"
   or "Enable Actions", click it. You should SEE the
   Actions page after that.
3. **First, look at the list of runs. Do not click Run yet.**
   - Uploading files to the `main` branch can start a
     build by itself. If you already SEE a run with a
     yellow dot (in progress) or a yellow circle, that
     is your build. Let that one finish. Do NOT start
     a second one.
   - If you do NOT see any run, go to the next step.
4. On the left side, under **All workflows**, click
   **Windows build**.
5. On the right side, click the **Run workflow** button.
   A small box will appear.
6. In that box, leave the branch as **main**, then click
   the green **Run workflow** button inside the box.
7. You should SEE a new run appear at the top of the list,
   with a yellow dot. That means the build has started.
8. **Now wait.** The first build can take 30 to 60 minutes
   on the screen, because one step compiles the Tauri tool
   from source. A long wait is normal. Do not click Run
   again, and do not close the page in a panic.
9. When it finishes, you should SEE either:
   - A **green tick** (a check mark) next to the run.
     That means the build worked. Go to Step 8.
   - A **red cross** next to the run. That means the
     build failed. Go to Step 7. Do NOT run it again.

**If it looks different:** if the **Actions** tab is not
there, or if you cannot find **Windows build** on the left,
go back to Step 5 and check the workflow file again. If
the file is there but Actions still does not show it,
stop and tell me.

---

## Step 7 — If it shows a red cross: copy the log, do not guess

1. Click the run with the red cross.
2. You should SEE a list of steps on the left, with red
   crosses next to the steps that failed. Click the failed
   step. The most likely ones are **Install Tauri CLI**,
   **Run tests**, or **Build Tauri app, NSIS installer
   and portable zip**.
3. You should SEE a log (many lines of text) for that step.
   Scroll to the very bottom of that log.
4. Copy the **last 50 lines** of that log. To do that,
   click and drag to select the last 50 lines, then copy
   them (Ctrl + C), then paste them into a message to
   Claude.
5. **Do NOT try to fix it yourself, and do NOT click Run
   workflow again.** Bring the log to Claude first. One
   failed run plus one log is enough to work out what
   went wrong. Re-running again and again wastes GitHub
   minutes and does not fix the problem.

**Remember the rule from the start of this guide: re-run
the workflow at most once before bringing the log to
Claude.** In practice, that means: if the first run fails,
bring the log to Claude, and only run it again if Claude
says to run it again.

---

## Step 8 — Download the build, check the fingerprints, install

Do this only if the run has a green tick.

1. Click the finished run with the green tick.
2. Scroll to the bottom of the page. You should SEE a
   section called **Artifacts**.
3. Click **fetchforge-windows-x64**. A zip file will
   download to your computer.
4. Right-click that zip file, click **Extract All**, then
   click **Extract**.
5. Inside the extracted folder, you should SEE two files:

   - `FetchForge-Setup-0.1.0-x64.exe` (the installer)
   - `FetchForge-portable-x64.zip` (the portable version)

**Check the fingerprints before you install:**

6. Open PowerShell (see Step 3 for how to open it).
7. Go to the folder with the two files, and run these two
   commands, one at a time:

   ```powershell
   Get-FileHash .\FetchForge-Setup-0.1.0-x64.exe -Algorithm SHA256
   Get-FileHash .\FetchForge-portable-x64.zip -Algorithm SHA256
   ```

8. Now go back to the green-tick run on GitHub, and click
   the step called **Print SHA-256 of each output file**.
   You should SEE four lines there, each with a long
   number and a file name.
9. Compare the numbers:
   - The installer number on your computer must match the
     installer line in the GitHub log, letter by letter.
   - The portable zip number on your computer must match
     the portable zip line in the GitHub log, letter by
     letter.
   - If either number does NOT match, do NOT install it.
     Stop and tell me. The download may be damaged.
   - If both numbers match, the files came from your own
     build, and it is safe to continue.

**About the Windows SmartScreen warning — please read this:**

10. Double-click `FetchForge-Setup-0.1.0-x64.exe` to install
    it.
11. Windows may show a blue window that says "Windows
    protected your PC" and "Microsoft Defender SmartScreen
    prevented an unrecognized app from starting". This
    happens because the installer is **unsigned** — that
    means no company has paid for a certificate to prove
    who made it. It does NOT mean the file is a virus.
    It means Windows does not know who made it.
12. **Only continue if BOTH of these are true:**
    - The file came from YOUR OWN GitHub workflow run
      (the green-tick run in YOUR private repository), and
    - The fingerprint numbers matched in step 9 above.
    If both are true, click **More info**, then click
    **Run anyway**.
    If either one is NOT true — for example, someone sent
    you the file, or the fingerprint did not match — do
    NOT click Run anyway. Stop and ask me.
13. Follow the installer. It installs for your user account
    only, and does not need admin rights. When it asks
    about deleting old FetchForge data on uninstall, the
    safe choice is to keep your data.
14. When the install finishes, open FetchForge from the
    Start menu. You should SEE the FetchForge app window
    with a list of downloads (empty at first) and a
    status line at the bottom.

**The 5-minute manual test — do this right after install:**

15. **The app starts the bridge.** Open FetchForge. In the
    status line at the bottom, you should SEE something
    like "Bridge: running". If it says the bridge is not
    running, or shows an error, write down exactly what
    it says and bring it to Claude. Do not try to start
    the bridge by hand yet.
16. **The Pair a browser button shows a code.** In the app,
    find the Pair section and click **Pair a browser**.
    You should SEE a code that looks like `XXXX-XXXX`
    (8 letters and numbers, with a dash in the middle),
    and a countdown of about 60 seconds. When the countdown
    reaches 0, the code should disappear. That means pairing
    works at the app end.
17. **Download a small test file.** In the app, add a
    download with a small, safe, public file link that you
    know (for example, a small file from a website you
    trust). You should SEE it appear in the list, go to
    Downloading, then Completed, and the file should appear
    in your Downloads folder, in a subfolder for its type.
    If it stays stuck on Downloading for more than a few
    minutes, or shows Failed, write down the exact error
    message and bring it to Claude.
18. **Pause and resume.** Start a slightly larger download,
    click Pause, wait a few seconds, then click Resume.
    You should SEE it continue from where it paused, not
    start from 0, and finish as Completed.
19. **Close and reopen.** Close FetchForge completely, then
    open it again. You should SEE your finished downloads
    still listed. That means the app saved its history.

If any of steps 15 to 19 does something different from what
is written here, write down exactly what you saw, step by
step, and bring it to Claude. Do not guess a fix.

---

## Step 9 — Load the extension in Chrome and in Firefox

The extension is the part that lets your browser send a
download to FetchForge. Load it by hand first, before you
try pairing it.

### In Chrome (or Edge, or any Chromium browser)

1. Open Chrome. In the address bar, type:

   `chrome://extensions`

   and press Enter. In Edge, type `edge://extensions`
   instead.
2. At the top right of the Extensions page, turn on
   **Developer mode**. You should SEE new buttons appear
   at the top, including **Load unpacked**.
3. Click **Load unpacked**.
4. A folder window will open. Go to the unzipped FetchForge
   folder on your computer, then open the folder called
   `extensions`, then open the folder called `chromium`,
   and click **Select Folder**.
5. You should SEE a new extension called **FetchForge** in
   the list.
6. Click **Details** under FetchForge, and look for the
   **ID**. You must SEE exactly this ID:

   `hkmnehbdedolcaaikibkadblmbfkkija`

   If the ID is different, stop. Do not pair it. Tell me
   the ID you see, because a different ID means a different
   extension was loaded.
7. Make sure the extension is turned on (the switch next
   to it should be on, not off).

### In Firefox

1. Open Firefox. In the address bar, type:

   `about:debugging`

   and press Enter.
2. On the left side, click **This Firefox**.
3. Click the button called **Load Temporary Add-on**.
4. A file window will open. Go to the unzipped FetchForge
   folder, then `extensions`, then `firefox`, and select
   the file called `manifest.json`. Click **Open**.
5. You should SEE **FetchForge** appear in the list of
   temporary extensions.
6. The Firefox ID for FetchForge is:

   `fetchforge@example.local`

   This is a development ID only. It must be replaced with
   a permanent ID before any public release, because
   changing it later would break the browser connection.
   For this test build, this ID is correct, and you should
   SEE it in the extension details if Firefox shows it.
7. Note: a temporary add-on in Firefox disappears when
   you close Firefox completely. You will need to load it
   again after a restart. That is normal for this test.

**If it looks different:** if Chrome says the extension
has errors, or Firefox refuses to load it, write down the
exact error message and bring it to Claude. Do not change
any file inside the extension folder to try to fix it.

**Pairing (only after Steps 8 and 9 work):** open
FetchForge, click **Pair a browser**, note the
`XXXX-XXXX` code, then open the extension popup in your
browser and enter the code there, following the popup's
own instructions. The extension first checks that it is
really talking to FetchForge before it trusts the code.
If pairing fails, write down exactly what the popup says
and bring it to Claude.

---

## Step 10 — Security rules, in plain words

These rules are short, and they matter more than any other
part of this guide.

1. **Never share your GitHub password with anyone.**
   Not with me, not with Claude, not in a chat, not in an
   email, not in a screenshot. GitHub will never ask you
   for your password in an email or a message. If a page
   asks for it outside the normal GitHub sign-in page,
   close that page.
2. **Never create a "personal access token" for anyone.**
   A personal access token is like a second password that
   lets someone else act as you on GitHub. No one working
   on FetchForge needs one from you. If someone asks you
   to create one and send it to them, do not do it. That
   is a common way accounts are stolen.
3. **Keep the repository Private.** Do not click the
   button that changes it to Public. A public repository
   lets anyone on the internet see your code and your
   build. For this test, Private is correct.
4. **Do not share the installer with other people yet.**
   It is unsigned, and it is a test build. Only install
   the copy that came from your own workflow run, after
   the fingerprint check in Step 8.
5. **If you uploaded a secret by mistake, act fast:**
   - If you uploaded a file with a password, a token, a
     key, or a `.env` file, do NOT just delete the file
     and think it is safe. GitHub keeps old versions.
   - Change that password or token right away, wherever
     it is used, so the old one stops working.
   - Then tell me, so we can clean the repository history
     properly. Deleting the file alone does not remove it
     from GitHub's history.
6. **If GitHub sends you a warning email about a secret
   or a strange sign-in, do not ignore it.** Read it,
   change your password if it says to, and tell me.

---

## Step 11 — What next

When you have a result, bring it back, in this order:

1. **The build result:** green tick or red cross, and if
   red, the last 50 lines of the failed step's log (see
   Step 7).
2. **The real-browser results:** what happened in the
   5-minute manual test in Step 8 (steps 15 to 19), what
   the Pair button showed, and what happened when you
   loaded the extension in Chrome and in Firefox (Step 9),
   including the extension ID you saw.
3. **Anything that looked different:** write down exactly
   what you saw, step by step, instead of guessing what
   it means.

That is the material for the next round of work (QW 2):
fixing whatever the Windows build and the real browser
showed, starting with the biggest problem first, and not
changing Linux-side code again until Tauri has actually
compiled on Windows.

Do not start a second build, do not change code, and do
not publish anything, until we have looked at these
results together.

---

All done. Here is your package and GITHUB_GUIDE.md,

- The source package zip: the clean zip built from the final
  commit in Step 5 (see the final report for its exact file
  name, file count, and SHA-256 number).
- This guide: `GITHUB_GUIDE.md`, in the top level of the
  FetchForge source package.
