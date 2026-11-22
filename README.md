<p align="center"><img src="assets/icon/sunset.png" width="128" alt="Maverick icon"></p>

# Maverick

Maverick is a personal desktop app for macOS, written in Rust with [GPUI](https://www.gpui.rs). It works with the Chrome you already use, through a small extension.

Today it lists every open Chrome tab, updates the list as tabs change, and switches Chrome to the tab you pick.

## Requirements

- macOS with Xcode installed.
- Rust 1.89 or later. The code is tested on Rust 1.100.
- Google Chrome 116 or later.

## Install

1. Download the `.dmg` for the latest version from [Releases](https://github.com/krishmula/maverick/releases). Builds are for Apple silicon.
2. Open it and drag **Maverick** into **Applications**. Start it from there, not from the disk image: Chrome stores the app's path when it first runs.
3. The app is not notarized by Apple, so macOS blocks the first launch. Open **System Settings > Privacy & Security** and click **Open Anyway**, or run:

   ```sh
   xattr -dr com.apple.quarantine /Applications/Maverick.app
   ```

## Run from source

```sh
cargo run --release -p maverick
```

Only one copy runs at a time. A second launch exits without opening a window.

## Connect Chrome

On each start, the app writes the extension to `~/Library/Application Support/dev.maverick.app/extension` and registers itself with Chrome. Load the extension once:

1. Open `chrome://extensions` in Chrome and turn on **Developer mode**.
2. In the app, click **Show folder**.
3. Drag the `extension` folder onto the extensions page.

The app shows your lists when Chrome connects. If the app starts after Chrome, Chrome reconnects within 30 seconds.

After an app update, Maverick asks the extension to reload itself. Extension 0.2.0 and older cannot do that: open `chrome://extensions` and click the reload arrow on **Maverick Bridge** once.

## The window

The bar at the top switches between two lists, **jobs** and **tabs**, and shows the link to Chrome. Click the link to measure the round trip again.

Under the bar is the band. Each cell is one posting from your tracker, oldest at the left:

- An orange cell is an open posting, one that is on the jobs list.
- A teal cell is a posting you applied to.
- Every other cell lights up only when a current drifts over it. A higher score makes a brighter cell.
- The selected job is the target. Lines run from it to the next best open jobs.

The list is on the left and the detail pane is on the right. The pane shows the selected row, and its fill report once it has one.

Four colors carry meaning, and nothing else in the window has a color:

| Color | Name | Meaning |
|---|---|---|
| Orange | Yours | It needs you: a field left open, the review, the Submit click. |
| Violet | Model | The model answered or drafted it. Check it. |
| Teal | Done | It worked. |
| Pink | Failed | Maverick tried and could not. The reason is next to it. |

## Change the app icon

Click the icon at the left of the bar and pick Stealth, Sunset, Arctic, Neon or Crimson. Maverick shows the new icon in the Dock right away and remembers it. Finder and Launchpad keep showing Sunset, the icon inside the app bundle.

## Use the tab list

- Click **tabs** in the bar.
- Type to filter tabs by title or address. Every word you type must match.
- Press the Up and Down arrow keys to move the selection.
- Press Enter, or click a row, to switch Chrome to that tab.

## Fill an application form

Maverick fills application forms on two systems. It never clicks Submit.

- Ashby, on `jobs.ashbyhq.com`.
- Greenhouse, on the boards Greenhouse hosts: `job-boards.greenhouse.io`, the older `boards.greenhouse.io`, and their `eu.` variants. A company's own careers page that shows a Greenhouse form inside it is not covered.

1. Click **jobs** in the bar. The list holds the open postings from your imported tracker, best score first.
2. Choose a job with a click or Enter. Maverick opens the form in a new Chrome tab and fills it.
3. Read the report in the detail pane. It has one square per form field, and it lists the fields left for you and the answers the model gave.
4. Press Enter again to bring the filled form forward. Answer what Maverick left, and click Submit yourself.

For a posting that is not in the list, open it in Chrome and click **Fill form** on that tab's row.

Answers come from two places:

- Your standard answers go in first. They are fixed rules, so the same question always gets the same answer.
- A Claude model then answers what the rules left, using only the `facts` in your answers file. It runs through the `claude` command you are signed in to, so there is no API key. With no facts in the file, no model is asked.

- For essay questions, the model writes a draft from the `background` in your answers file, which holds your resume as text, and from the job description. Read each draft before you submit. With no background in the file, essays are left for you.

The model leaves pay questions, demographic questions and agreements for you. Maverick lists every answer and every draft the model gave, with what it used, and every field it left.

On a Greenhouse form, Maverick leaves the demographic, equal opportunity and education sections alone. It splits `name` into the First Name and Last Name fields at the first space, unless a rule covers those titles.

After a fill, Maverick watches the tab for the system's confirmation. When you submit, it marks the posting as applied in the local database, records the application, and takes the job off the list. The watch stops after 30 minutes or when the tab closes.

Your standard answers live in `~/Library/Application Support/dev.maverick.app/answers.json`. The file is read on every fill:

```json
{
  "name": "Ada Example",
  "email": "ada@example.com",
  "phone": "+1 415 555 0100",
  "resume": "/Users/ada/Documents/resume.pdf",
  "location": "Springfield, Illinois",
  "rules": [
    { "any": ["linkedin"], "answer": "https://www.linkedin.com/in/ada-example" },
    { "any": ["require sponsorship"], "not": ["without"], "answer": "no" },
    { "any": ["start date"], "answer": "2027-01-18" }
  ],
  "facts": [
    "Lives in Springfield, Illinois, in the Central time zone.",
    "Citizen of Canada. Will need visa sponsorship in the future."
  ],
  "background": [
    "Software Engineer at Example Robotics, 2023 to present.",
    "Built a job queue in Rust that cut nightly batch time from 6 hours to 40 minutes."
  ]
}
```

A rule answers every question that holds one of its `any` phrases and none of its `not` phrases. A yes or no question takes `yes` or `no`, a date takes `YYYY-MM-DD`, and a question with options takes the text of one option. `location` is optional.

## Fill a Workday application

Maverick also fills applications on Workday sites, at `*.myworkdayjobs.com` and `*.myworkdaysite.com`. A Workday application has several pages, and Maverick fills one page at a time.

1. Choose the job in Maverick, or click **Fill form** on the posting's tab. Maverick opens the application and chooses **Apply Manually**.
2. Most employers then ask for an account. Maverick stops and tells you so. Sign in to Workday in that tab yourself. Maverick never creates an account, never types a password and never signs in.
3. On each page of the application, click **Fill form** on the tab's row. Maverick fills the page that is on screen.
4. Review the page and click **Save and Continue** yourself. Maverick never clicks it, and it never submits.

Maverick fills text fields, the email and the phone number, drop-down lists, radio groups, check boxes and the resume upload. It leaves these for you:

- Search lists, such as "How Did You Hear About Us?" and the country phone code.
- Dates.
- The entries of your work history and your education.
- Every field on the voluntary disclosure and self-identification pages.

Each fill saves the structure of the page to `~/Library/Application Support/dev.maverick.app/captures/workday-<time>.json`: the questions, the kind of each control, and the option labels. The file holds none of your answers. Maverick's Workday support was written without a Workday account, and these files show what real pages look like.

Check every answer before you continue. This is how far each part is verified:

| Part | Verified against |
|---|---|
| Posting addresses, the job's details, opening the application, the sign-in stop | Live Workday sites |
| Reading a page's fields, and filling text, an email, a phone number, a radio group, a check box and a drop-down list | The first page of two live applications that ask for no account, with made-up answers that were never saved |
| Later pages, a signed-in session, the resume upload, groups of check boxes and long text | A page rebuilt from Workday's public scripts and public notes. No live page |
| Noticing that you submitted | Nothing. Maverick may not mark a Workday posting as applied |

## Import the job tracker

The job funnel keeps its records in a local SQLite database at `~/Library/Application Support/dev.maverick.app/maverick.db`. To copy the tracker spreadsheet into it, export the Google Sheet as `.xlsx` and run:

```sh
cargo run --release -p maverick -- import-tracker "/path/to/tracker.xlsx"
```

The import reads three tabs: `Sheet1` (applications), `Found` (postings), and `Hiring Posts`. It updates records the database already holds, so you can repeat it after the sheet changes. It lists each row it cannot read and leaves that row out.

## Check the rules against stored decisions

Code rules skip a posting from its title and location alone, and spot a posting that repeats an earlier one. To compare them with the decisions the database already holds, run:

```sh
cargo run --release -p maverick -- check-rules
```

The report counts what the rules would skip and lists each posting where the rules and the stored decision disagree.

## Run the tests

```sh
cargo test --workspace
```

Four tests drive a real browser and do not run by default. They need a [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/) binary, because Google Chrome no longer accepts `--load-extension`:

```sh
MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome -- --ignored --nocapture
```

The third test fills a live Ashby form with made-up answers and a blank PDF, and never submits. It also needs a posting's address:

```sh
MAVERICK_E2E_ASHBY="https://jobs.ashbyhq.com/<company>/<posting id>" \
	MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome a_live_ashby -- --ignored --nocapture
```

The fourth test does the same on a live Greenhouse form:

```sh
MAVERICK_E2E_GREENHOUSE="https://job-boards.greenhouse.io/<board>/jobs/<posting id>" \
	MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome a_live_greenhouse -- --ignored --nocapture
```

Three more tests cover Workday. One fills a page rebuilt to look like a Workday application and runs with the first command above. The other two reach live sites and need a posting's address:

```sh
MAVERICK_E2E_WORKDAY="https://<company>.wd1.myworkdayjobs.com/<site>/job/<place>/<posting id>" \
	MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome a_live_workday_posting -- --ignored --nocapture
```

That test reads the job, opens the application and expects the sign-in stop. It types nothing. For a posting whose application asks for no account, set `MAVERICK_E2E_WORKDAY_OPEN` and run `a_live_workday_page`. It fills the first page with made-up answers, uploads nothing and never leaves the page.

## Make a release

1. Raise `version` in `app/Cargo.toml` and merge it into `main`.
2. Tag that commit and push the tag:

   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```

The `Release` workflow builds `Maverick.app` and attaches a `.dmg` and a `.zip` to a new GitHub release. The tag must match the version. To build the same files locally, run `scripts/bundle.sh`; they land in `target/dist`.

Each icon is an SVG in `assets/icon`. After editing one, run `assets/icon/build.sh` (needs `brew install librsvg`) and commit the PNGs and `Maverick.icns` it writes. Sunset is the bundle icon.

## Layout

| Path | Contents |
|---|---|
| `core/` | The `maverick-core` crate. It has no UI code. It holds the bridge to Chrome, page control, the form filler, the relay, the extension installer, and the job funnel's database and tracker import. |
| `app/` | The `maverick` crate. It draws the window with GPUI and [GPUI Kit](https://gpui-kit.com). `app/src/ui.rs` holds the Design System: the fonts, the colors and the parts every view shares. |
| `extension/` | The Chrome extension. It reports tabs and runs the commands the app sends. |

## How the app talks to Chrome

Chrome starts the `maverick` binary as a native messaging host and passes the extension's origin as the first argument. In that mode the binary is a relay. The relay copies bytes between Chrome's pipes and a Unix socket that the running app listens on.

The app opens no network port. Chrome lets only this extension start the relay, and only the current user can open the socket.

## How the app acts on a page

`core/src/page.rs` sends Chrome DevTools Protocol commands to one tab. The extension passes them on with `chrome.debugger`, so this works in the Chrome you already use, on pages where you are signed in.

- `fill` types text into a field. `click` uses the mouse. The page sees both as real input.
- `upload` puts a file from this computer into a file input.
- `navigate` opens an address and waits for the page to load.
- `press` sends a key such as Enter, and `wait_for` waits for an element to appear.

Chrome shows a bar that says Maverick Bridge is debugging the browser while a tab is attached. Closing the bar detaches the tab. The app attaches only while it acts on a tab.

