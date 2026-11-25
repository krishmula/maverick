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

The bar at the top switches between the **jobs** tab, the **tabs** list, the **mav** view and the **posts** tab, and shows the link to Chrome. **connections** at the right end sets up Google, TickTick, the model and Chrome. The number next to **jobs** counts the postings that wait on a decision. The number next to **mav** counts the TickTick tasks that are overdue or due today. The number next to **posts** counts the hiring-post notes that wait on you. Click the link to measure the round trip again.

On the jobs tab and the tabs list, the band is under the bar. Each cell is one posting from your tracker, oldest at the left:

- An orange cell is an open posting, one that waits on a decision on the jobs tab.
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

Maverick fills application forms on two systems. It clicks Submit only when you turn on auto-submit (see [Auto-submit](#auto-submit)).

- Ashby, on `jobs.ashbyhq.com`.
- Greenhouse, on the boards Greenhouse hosts: `job-boards.greenhouse.io`, the older `boards.greenhouse.io`, and their `eu.` variants. A company's own careers page that shows a Greenhouse form inside it is not covered.

1. Click **jobs** in the bar and approve a posting. Maverick queues it and fills its form in a background tab, several at once (see [The apply queue](#the-apply-queue)).
2. Read the report in the detail pane. It has one square per form field, and it lists the fields left for you and the answers the model gave.
3. Press Enter to bring the filled form forward. Answer what Maverick left, and click Submit yourself.

For a posting that is not in the list, open it in Chrome and click **Fill form** on that tab's row.

Answers come from two places:

- Your standard answers go in first. They are fixed rules, so the same question always gets the same answer.
- A Claude model then answers what the rules left, using only the `facts` in your answers file. It runs through the `claude` command you are signed in to, so there is no API key. With no facts in the file, no model is asked.

- For essay questions, the model writes a draft from the `background` in your answers file, which holds your resume as text, and from the job description. Read each draft before you submit. With no background in the file, essays are left for you.

The model leaves pay questions, demographic questions and agreements for you, unless your answers file says otherwise under `standing` (see [Auto-submit](#auto-submit)). Maverick lists every answer and every draft the model gave, with what it used, and every field it left.

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
  ],
  "target": [
    "Full-time backend or infrastructure jobs, new grad to mid-level, anywhere in the United States."
  ]
}
```

A rule answers every question that holds one of its `any` phrases and none of its `not` phrases. A yes or no question takes `yes` or `no`, a date takes `YYYY-MM-DD`, and a question with options takes the text of one option. `location` is optional.

## Fill a Workday application

Maverick also fills applications on Workday sites, at `*.myworkdayjobs.com` and `*.myworkdaysite.com`. A Workday application has several pages, and Maverick fills one page at a time.

1. Choose the job in Maverick, or click **Fill form** on the posting's tab. Maverick opens the application and chooses **Apply Manually**.
2. Most employers then ask for an account. Maverick signs in with the sign-in you stored, as the next section describes. With none stored, it stops and tells you how to store one, and you can sign in to Workday in that tab yourself.
3. On each page of the application, click **Fill form** on the tab's row. Maverick fills the page that is on screen.
4. Review the page and click **Save and Continue** yourself. Maverick never clicks it, and it never submits.

### Sign in to Workday

Each employer has its own Workday site with its own accounts. Maverick uses one email address and one password for all of them, and reads them from your macOS Keychain. Store them once, in Terminal:

```sh
security add-generic-password -s dev.maverick.workday -a <email> -w
```

Type the password at the prompt. It does not show, and it stays out of your shell history. To change it later, add `-U` to the same command. Maverick reads the item with `/usr/bin/security` on each fill that meets a sign-in page, keeps it only for that fill, and never writes it to a file, a log or a message.

When an application asks for an account, Maverick does this, once per fill:

1. It types the email address and the password into the sign-in form and presses **Sign In**.
2. If Workday refuses, it tries once to create your account at that employer, with the same email address and password. Workday gives the same answer for a wrong password and for an email address it does not know, so Maverick cannot tell the two apart. It ticks the consent box when the form has one.
3. If Workday asks you to verify your email address, Maverick stops and says so. Open the email from Workday, verify, then click **Fill form** again. Maverick never reads your email.
4. If Workday also refuses the new account, Maverick stops and shows you what Workday said. It never tries the sign-in or the new account a second time in one fill.

The report of a fill says **Signed in to Workday** or **Created your Workday account** when Maverick did either.

Nobody has pressed **Sign In** or **Create Account** on a live Workday site with this code. What Maverick does up to the press is verified on live sites with made-up values. What it does after the press follows Workday's public page script and is tested only against a page rebuilt from it. Watch the first real sign-in.

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
| Reading the stored sign-in from the Keychain | The real Keychain, with made-up items that the test stores and deletes |
| Finding the sign-in form and the create-account form, typing into them, ticking the consent box, finding the button to press | The sign-in pages of 19 employers, with a made-up email address and password. No button was pressed |
| Everything after a press of **Sign In** or **Create Account**: a good sign-in, a refusal, the new account, the request to verify your email address | A page rebuilt from Workday's public script. No live page |
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

## Connect Google accounts

Maverick can connect any number of Google accounts and use Sheets, Drive, Gmail and Calendar for each one. Calendar access includes adding events, which Mav uses for planned blocks. It signs in with your own OAuth client, so first create one:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a project and enable the Google Sheets, Google Drive, Gmail and Google Calendar APIs.
2. Set up the OAuth consent screen as **External**, add each account you will connect as a test user, and publish the app. Sign-ins to an app that stays in testing expire after 7 days.
3. Create a client of type **Desktop app**.

Then store the client and connect each account:

```sh
maverick google client <client id> < file-with-the-client-secret
maverick google add      # opens the Google sign-in in your browser; repeat for each account
maverick google list
maverick google check    # proves that each account still signs in
maverick google remove <email>
```

The client secret and each account's refresh token are kept in the macOS Keychain, under `dev.maverick.google-client` and `dev.maverick.google`. The list of accounts, which holds no secret, is `google-accounts.json` in the data directory. Google warns that the app is not verified. That warning is expected for a client you made yourself.

## Talk to Mav

The **mav** view is a conversation with Mav, your planning assistant. Type what to remember or plan and press Enter, for example "remind me to follow up with the recruiter Tuesday at 10" or "find me 2 hours tomorrow for take-home prep".

Mav proposes each TickTick task or calendar block as a card. Nothing is written until you click **Confirm** on the card. **Skip** drops it. A block that overlaps a busy event lists the clash in pink before you confirm. Questions such as "what is left today?" get an answer and no cards.

The left side lists your chats with Mav, newest first. Maverick saves every chat in its local database as you go; click one to read it again, **+ new** to start another, and **delete** on the open one to remove it. Mav reads only the open chat. A proposal in a saved chat shows how it ended (added, skipped or failed) and cannot be confirmed again, since your tasks and calendar have moved on since.

The right side shows the calendar, then every open TickTick task in the lists Mav shows, grouped by priority (high, medium, low, none) or, with the **date** switch, into overdue, today, the coming week and no date. On the right side, without Mav:

- Tick a task's box to finish it. It shows as done until the next read drops it.
- Click a task's priority tag to move it to the next priority: high, medium, low, none, then high again.
- Type in **Add a task** and press Enter to add it to your default list. Click the tag next to the box to choose its priority first.
- The calendar shows one day with its events and the free time between them, from 8:00 to 22:00. **‹** and **›** move a day at a time through the coming week, **today** comes back, and **week** lists the coming seven days. Each event shows its profile.
- **+ block** on a free stretch asks for a name; press Enter and Maverick adds the block to the profile marked for new blocks.
- **delete** on an event asks **sure?** first; a second click deletes it from Google Calendar.
- **collapse ›** folds the side panel into a strip with the count of overdue and due-today tasks; **‹** opens it again. Maverick remembers both choices in `mav-view.json`.

The number next to **mav** in the bar counts the tasks that are overdue or due today.

Mav can also finish, change, move and delete tasks you already have: "mark the lease done", "make the quiz prep high priority and move it to Thursday at 6", "move the dentist task to Personal". Mav also moves and deletes calendar events: "push my next event back an hour" keeps the event's length unless you name an end. Each one comes as a card that shows what changes, such as `low → high`, and runs only when you confirm it.

When an application goes through, Mav offers a task to follow up a week later at 10:00, in a list whose name contains "job", or else the Inbox.

Mav runs the `claude` command you are signed in to, like the form filler. To see what Mav would propose without the window, and without writing anything:

```sh
maverick mav "find me 2 hours tomorrow for take-home prep"
```

### First start

The first time Maverick opens, a guide walks through four steps: choose a model, connect TickTick, connect Google, and connect Chrome. Only the model is required; the rest can be skipped, and **later** closes the guide. **run setup again** under **connections** opens it again. Each step shows the same controls as **connections**.

### Connections

**connections** in the bar shows everything Maverick is linked to:

- **Google profiles.** Before the first account, paste the ID and secret of your Google OAuth client (a Desktop app client from the Google Cloud console; see [Connect Google accounts](#connect-google-accounts)). Up to 5 Google accounts, each named by you, such as Work or School. Click a profile to rename it, choose which of its calendars Mav reads (the primary one unless you choose others), and mark the one profile new calendar blocks go to. Mav knows the names, so "put it on my school calendar" goes to School. **sign in again** in orange means the account has not granted something Maverick now asks for, such as adding events. **+ add account** opens the Google sign-in in your browser.
- **TickTick.** Paste your API token to connect. Choose the list new tasks go to, and which lists Mav shows.
- **Models.** Which model providers can run, and which provider and model each job uses: Mav chat, form filling and scoring. See [Choose the models](#choose-the-models).
- **Chrome.** Whether Chrome is connected.

### Choose the models

Maverick can run its model calls through a command line tool you are signed in to, or through an API key:

| Provider | Kind | Set up |
|---|---|---|
| Claude Code | command | install it and run `claude` once to sign in |
| Codex | command | `npm i -g @openai/codex`, then `codex login` |
| OpenCode | command | install from opencode.ai, then `opencode auth login` |
| Anthropic | API key | create a key at platform.claude.com |
| OpenAI | API key | create a key at platform.openai.com |
| OpenRouter | API key | create a key at openrouter.ai |

Under **connections**, paste an API key and press Enter: Maverick checks it with the provider before it counts as ready, and keeps it in the macOS Keychain under `dev.maverick.api-key`. **test** sends a command line tool a tiny question. For each job, click a provider and optionally type a model; an empty model means the provider's default (Claude Code uses `sonnet`, Anthropic `claude-opus-5-5`, OpenAI `gpt-5-mini`, OpenRouter `openrouter/auto`, Codex and OpenCode their own). Without a choice, every job uses Claude Code with `sonnet`. The choices are saved as `models.json` in the data directory.

From the terminal:

```sh
maverick models list                        # what is installed or keyed, and what each job uses
maverick models check [provider]            # a real test of one provider, or all of them
maverick models key openrouter < key-file   # stores and checks an API key
maverick models use scoring codex [model]   # jobs: mav, forms, scoring
```

### Connect TickTick

Maverick uses a personal API token. In TickTick on the web, open **Settings**, then **Account**, then **API Token**, and create one. Paste it under **connections**, or store it from the terminal:

```sh
maverick ticktick token < file-with-the-token   # checks the token with TickTick, then stores it
maverick ticktick today                         # prints the same agenda as the mav view
maverick ticktick forget
```

The token is kept in the macOS Keychain under `dev.maverick.ticktick`.

### Connect your calendar

Mav reads the calendars you chose in each Google profile under **connections** (see also [Connect Google accounts](#connect-google-accounts)). It adds blocks to the profile you name, or else to the one marked for new blocks. An account connected before that permission was added only allows reading: click **sign in again** on its profile.

Free time is counted between 8:00 and 22:00. Events marked free do not block time.

## Link the live tracker

To use the Google Sheet itself as the tracker in place of the `.xlsx` import, link it by part of its name:

```sh
maverick tracker link "Tracker"
```

From then on, each funnel pass does the following:

1. It reads the sheet's three tabs into the database. The sheet is the source of truth.
2. It adds the postings intake found as new rows in the Found tab.
3. It scores what is still found and writes each decision back: Status, Score, Variant, Flags and Notes.

A decision goes only into a row whose Status still says Found when Maverick writes. So if another scorer works on the same sheet, Maverick never overwrites its decisions.

## Approve or skip postings

**jobs** in the bar opens the jobs tab, and the window opens on it. The number next to it counts the postings that wait on a decision.

- **Pending** lists the postings in Review or Found that you have not applied to. Each card shows the score, the company, the application system, the variant, the found date, the location, the flags, a sponsorship badge (sponsors, no sponsorship, or unknown), and who you can reach at the company. See [Reach someone at the company](#reach-someone-at-the-company).
- Sort by **latest** found, by **top score** (apply-ready first, the default), or by **outreach** (postings with someone to reach first, then by top score). Decided rows always sort below pending ones.
- Press `a` to approve the selected posting and `s` to skip it, or click **approve** or **skip**. The selection moves to the next pending posting. `⌘Z` or **undo** takes back the last decision.
- **Submitted** lists the applications, latest first or by company.

### Auto-submit

Click **auto-submit off** in the board's toolbar to turn auto-submit on. It is off until you do. With it on, Maverick presses Submit on a form once, when all of these hold:

- Every required field holds one of your standard answers, or an answer the model worked out from your `facts`.
- No answer is a draft. A form with an essay waits for you to read the draft.
- Every answer went in, and the model could be asked.
- The page shows exactly one Submit button.

After the press, Maverick waits a minute for the system's confirmation and records the application. When the system asks for a code it emailed you, or checks that you are human, the tab waits for you. Maverick never presses Submit twice.

Three entries in your answers file decide answers that would otherwise always be left for you:

```json
"legal_name": { "first": "Augusta Ada", "last": "Example" },
"standing": { "agrees_to_terms": true, "pay": "posted-midpoint" }
```

- `legal_name` answers a question that asks for your legal name. Every other name question takes `name`.
- `agrees_to_terms` lets the model agree to a form's terms, privacy notice and statement that the application is true. This includes an arbitration agreement.
- `pay` set to `posted-midpoint` answers a pay question with the middle of the range the posting states. A posting without a range leaves the question for you.
- `pay_without_a_range` is the yearly pay in US dollars that a required pay question gets when the posting states no range. An optional pay question stays empty.

Two more things come from outside the standing answers:

- `transcript`, beside `resume`, is the file a question about a transcript gets.
- A question that asks whether you applied to the company before is answered from the applications Maverick has on record.

To see which required questions your answers do not cover yet, run `maverick gaps`. It reads the form of every open Ashby and Greenhouse posting, opens nothing in Chrome and writes nothing.

### The apply queue

Approving a posting puts it in the apply queue. Maverick fills queued forms several at once, each in its own background tab in Chrome, three at a time unless you change it with **-** and **+** in the board's toolbar. It submits a form only with auto-submit on. Each row shows where its posting is:

- **queued**: waiting for a free slot. The queue waits while Chrome is not connected.
- **opening** and **filling**: the form is loading, then the answers go in.
- **ready to submit**: every required field is answered. With auto-submit on, the pane says why Maverick left the Submit to you.
- **submitting**: nothing was left for you, and Maverick pressed Submit.
- **needs you**: required fields are left, there is a sign-in wall or an email to verify, or the posting is on a system Maverick cannot fill yet (Amazon, Oracle, Lever, company sites, unresolved Simplify links). Those open in a tab for you.
- **applied**: the application system confirmed your submission, and the posting moves to Submitted.
- **failed**: the reason shows in the pane. Approve it again to try again.

Select a posting that is ready or needs you and press Enter, or click **review in chrome**, to bring its tab forward. Approvals from before Maverick last started are not queued by themselves: click **apply approved** to queue them.

Decisions stay in the local database, apart from the postings, so a sheet sync or a tracker import never undoes them. They are not written to the sheet: the sheet only takes decisions on rows that still say Found, and an approval has no sheet status.

### Reach someone at the company

Every hour while the app is open, Maverick looks on LinkedIn for people at the companies of the postings to review, apply-ready first. It never sends anything.

- Each card's last line names the first person found, such as a connection, a team member or the hiring manager, or says there is no one, the lookup was deferred, or the posting is not checked yet. A card with a note shows a **note** badge with where the note stands.
- The detail pane lists the people. Click **draft note** to have the model write a connection note to the best one.
- Read the draft in the pane. Click **approve, type it** to have Maverick type it into LinkedIn in your Chrome, then click Send yourself and **mark sent**. You can also **edit** it, **skip** it, or rewrite it as an InMail with **use InMail**.

## Answer hiring posts

**posts** in the bar opens the posts tab. Every two hours while the app is open, Maverick runs five LinkedIn searches for posts about hiring software, backend and ML infrastructure engineers and new grads. It scrolls each result page up to three times and reads at most 40 new posts. It drafts a note to the poster for each post that fits, up to 10 a day. Each draft works the same way as a draft on the jobs tab: approve it, and Maverick types it, and you click Send. **check and sweep now** runs the people lookup and the post search at once.

Below the drafts, **found, no draft** lists the newest 100 hiring posts that were not skipped and have no draft, from Maverick's searches and from the tracker's `Hiring Posts` tab. Each one shows why it has no draft, and **open post** opens it in your browser.

**draft** on a found post writes a message to its author. Maverick opens the post in a background tab to read it and to find the author. A post that the tracker links to a search page has no **draft**, because Maverick cannot tell who posted it.

Maverick picks the form of every draft by who it goes to:

- A connection gets a message.
- Anyone else gets an InMail while an InMail credit is free, and a connection note when none is. LinkedIn's message window shows the credits left. Maverick reads that number there, and an InMail draft that is not sent yet holds one credit.
- A person whose message window says **Free message** gets an InMail, which uses no credit.

An InMail is 50 to 100 words with a subject. A connection note is at most 280 characters. When Maverick types a message or an InMail, it attaches the resume named in `answers.json`. Remove it in LinkedIn before you click Send if you do not want it.

### Make drafts sound like you

Put your rules and messages you sent in `~/Library/Application Support/dev.maverick.app/voice.json`. Every draft is written from them. Without the file, drafts are written from your resume alone.

```json
{
  "rules": ["End on one ask.", "Never mention visa status."],
  "examples": [
    { "slot": "note", "about": "a recruiter who posted the role", "text": "Hi Bo, I saw your post about the Backend Engineer role. Would love to connect." },
    { "slot": "inmail", "about": "an engineer; got a reply", "subject": "Backend Engineer, Payments", "text": "Hi Ada,\n\nI saw your post.\n\nThanks,\nSam" }
  ]
}
```

A note is written from your notes. A message and an InMail are written from your messages and InMails. A draft says you applied only when your applications hold a role at that company.

To read a draft without saving it, run `maverick linkedin try-draft <post link> <note|message|inmail>`. The post must be one Maverick has read.

## Find new postings

While the app is open, it runs the funnel every hour from 8 AM to 11 PM Pacific time, starting a minute after launch. Each pass pulls new postings and then scores what is found. The bar shows what the last pass did, and the jobs tab updates. A pass never submits an application.

Intake reads [Simplify's new-grad list](https://github.com/SimplifyJobs/New-Grad-Positions), which is also the source of the SWE List email, about a day earlier than the email. It adds open Software and AI/ML/Data listings from the last 72 hours that the database does not hold yet. It records each one by its Simplify page and keeps the original application address.

To run one pass now at any hour, run:

```sh
cargo run --release -p maverick -- run
```

Notify.careers postings still arrive only by email, so they still come in through the tracker import.

## Score found postings

Scoring reads the description of each posting still marked found and decides whether it is worth applying to:

```sh
cargo run --release -p maverick -- score
```

A run takes these steps, cheapest first:

1. It marks a posting that repeats an earlier posting or an application as a duplicate.
2. It skips a posting the title and location rules rule out.
3. It reads the description over plain HTTP, for at most 30 postings. It can read Simplify, Ashby, Greenhouse, Lever, Workday and Amazon postings. For a Simplify copy, it also records the original posting's address, so Maverick can fill that posting's form.
4. A Claude model, run through the `claude` command, scores the fit from 0 to 100 against `background` and `target` in your answers file. It also reads the facts that the rules in code act on: sponsorship, citizenship or clearance, minimum years, frontend focus and US location.
5. Code decides. A posting is skipped if it requires citizenship or clearance, requires 5 or more years, is mainly frontend, has no US location, or scores under 60. It is apply-ready at 75 or more with no blocking flag, such as "no sponsorship". All other postings go to review. A closed posting is skipped.

A posting that cannot be read or scored stays found, with the reason in its notes, and the next run tries it again. Add `--dry-run` to see the decisions without writing them, and `--limit <count>` to change the cap.

To compare the scorer with the scores the database already holds, run `check-scores [count]`. It scores the newest postings again, writes nothing, and reports how often the two decisions agree.

## Get notified

Maverick shows a macOS notification when:

- a queued form finishes filling and is ready to submit, needs your answer, or failed,
- the hourly check scores new apply-ready postings,
- the hiring-post sweep drafts new notes.

macOS shows them only while Maverick is not the frontmost app. The first start asks for permission. To change it later, open System Settings > Notifications > Maverick.

Notifications work only from `Maverick.app` in `/Applications` or `~/Applications`. A build run with `cargo run`, or an app in a temporary folder, shows none. To check the setup, run `/Applications/Maverick.app/Contents/MacOS/maverick notify`.

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

Six more tests cover Workday. Three run with the first command above, against rebuilt pages: one fills a page rebuilt to look like a Workday application, and two take a page rebuilt to look like Workday's sign-in through the sign-in and the new account, with a made-up sign-in in place of the Keychain. The other three reach live sites and need a posting's address:

```sh
MAVERICK_E2E_WORKDAY="https://<company>.wd1.myworkdayjobs.com/<site>/job/<place>/<posting id>" \
	MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome a_live_workday_posting -- --ignored --nocapture
```

That test reads the job, opens the application and expects the sign-in stop. It types nothing. With the same address, `a_live_workday_wall` types a made-up email address and password into the sign-in form and the create-account form, and presses nothing. No test reads your stored sign-in, and none presses **Sign In** or **Create Account** on a live site. For a posting whose application asks for no account, set `MAVERICK_E2E_WORKDAY_OPEN` and run `a_live_workday_page`. It fills the first page with made-up answers, uploads nothing and never leaves the page.

`cargo test --workspace` also stores a made-up item in your login Keychain under `dev.maverick.test-<number>`, reads it back and deletes it.

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

