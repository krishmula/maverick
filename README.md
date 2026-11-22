# Maverick

Maverick is a personal desktop app for macOS, written in Rust with [GPUI](https://www.gpui.rs). It works with the Chrome you already use, through a small extension.

Today it lists every open Chrome tab, updates the list as tabs change, and switches Chrome to the tab you pick.

## Requirements

- macOS with Xcode installed.
- Rust 1.89 or later. The code is tested on Rust 1.100.
- Google Chrome 116 or later.

## Run the app

```sh
cargo run --release -p maverick
```

Only one copy runs at a time. A second launch exits without opening a window.

## Connect Chrome

On each start, the app writes the extension to `~/Library/Application Support/dev.maverick.app/extension` and registers itself with Chrome. Load the extension once:

1. Open `chrome://extensions` in Chrome and turn on **Developer mode**.
2. In the app, click **Show folder**.
3. Drag the `extension` folder onto the extensions page.

The app shows your tabs when Chrome connects. If the app starts after Chrome, Chrome reconnects within 30 seconds.

After an app update, Maverick asks the extension to reload itself. Extension 0.2.0 and older cannot do that: open `chrome://extensions` and click the reload arrow on **Maverick Bridge** once.

## Use the tab list

- Type to filter tabs by title or address. Every word you type must match.
- Press the Up and Down arrow keys to move the selection.
- Press Enter, or click a row, to switch Chrome to that tab.

## Fill an application form

Maverick fills application forms on `jobs.ashbyhq.com`. It never clicks Submit.

1. Click **Jobs** in Maverick. The list holds the open postings from your imported tracker, best score first.
2. Choose a job with a click or Enter. Maverick opens the form in a new Chrome tab and fills it.
3. Review the form in Chrome, answer what Maverick left, and click Submit yourself.

For a posting that is not in the list, open it in Chrome and click **Fill form** on that tab's row.

Answers come from two places:

- Your standard answers go in first. They are fixed rules, so the same question always gets the same answer.
- A Claude model then answers what the rules left, using only the `facts` in your answers file. It runs through the `claude` command you are signed in to, so there is no API key. With no facts in the file, no model is asked.

- For essay questions, the model writes a draft from the `background` in your answers file, which holds your resume as text, and from the job description. Read each draft before you submit. With no background in the file, essays are left for you.

The model leaves pay questions, demographic questions and agreements for you. Maverick lists every answer and every draft the model gave, with what it used, and every field it left.

After a fill, Maverick watches the tab for Ashby's confirmation. When you submit, it marks the posting as applied in the local database, records the application, and takes the job off the list. The watch stops after 30 minutes or when the tab closes.

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

Three tests drive a real browser and do not run by default. They need a [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/) binary, because Google Chrome no longer accepts `--load-extension`:

```sh
MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome -- --ignored --nocapture
```

The third test fills a live Ashby form with made-up answers and a blank PDF, and never submits. It also needs a posting's address:

```sh
MAVERICK_E2E_ASHBY="https://jobs.ashbyhq.com/<company>/<posting id>" \
	MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome a_live -- --ignored --nocapture
```

## Layout

| Path | Contents |
|---|---|
| `core/` | The `maverick-core` crate. It has no UI code. It holds the bridge to Chrome, page control, the form filler, the relay, the extension installer, and the job funnel's database and tracker import. |
| `app/` | The `maverick` crate. It draws the window with GPUI and [GPUI Kit](https://gpui-kit.com). |
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

