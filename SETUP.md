# Setup

These steps take a stock reMarkable Paper Pro Move to a working install. Allow about an hour, plus time to create the Google and Todoist credentials.

## 1. Back up the tablet

Turning on developer mode runs a factory reset that deletes everything on the device. You don't have SSH access yet, so back up through reMarkable's own tools:

- Check in Settings that every notebook and document has synced to your reMarkable cloud account.
- Export anything you can't lose as a PDF from the reMarkable desktop or mobile app.

## 2. Turn on developer mode

Follow [reMarkable's developer mode instructions](https://developer.remarkable.com/documentation/developer-mode). The menu path is Settings > General > Paper Tablet > Software > Advanced > Developer Mode, though the wording can differ between OS versions. Don't interrupt power while the tablet resets.

Afterwards, find the root password under Settings > General > Help > About > Copyrights & Licenses > General Information. Each factory reset generates a new one.

## 3. Connect over SSH

Plug the tablet into your computer with USB-C:

```sh
ssh root@10.11.99.1
```

Add your SSH key so you don't have to type the password for each command:

```sh
ssh-copy-id root@10.11.99.1
```

Set the tablet's time zone. It affects which events count as today:

```sh
ssh root@10.11.99.1 'timedatectl set-timezone Europe/London'
```

Replace `Europe/London` with your own zone. Check it again after a factory reset.

## 4. Check the OS version

```sh
ssh root@10.11.99.1 'cat /etc/os-release'
```

OS 3.27.1.0 is the only version this project has been tested on. On a different version, check that the current XOVI and AppLoad releases support it before you continue.

## 5. Install XOVI and AppLoad

This project uses three things that belong to other people, none of them distributed here:

- [XOVI](https://github.com/asivery/xovi), which loads extensions into the tablet's interface
- [AppLoad](https://github.com/asivery/rm-appload), which adds the app launcher
- [xovi-tripletap](https://github.com/rmitchellscott/xovi-tripletap), which lets you start XOVI by pressing the power button three times

The [Vellum](https://github.com/vellum-dev/vellum-cli) package manager installs all three. Install Vellum with the two commands in its README, run on the tablet, then add the packages:

```sh
ssh root@10.11.99.1 '/home/root/.vellum/bin/vellum update && /home/root/.vellum/bin/vellum add appload tripletap'
```

If you'd rather use a desktop app, [reManager](https://github.com/rmitchellscott/reManager) installs the same packages.

SSH can stop responding right after this install. If it does, restart the tablet from its power menu.

Build XOVI's hash table, which you need to do once for each OS version. The interface stops for about a minute, and if the tablet asks for your passcode, enter it there:

```sh
ssh root@10.11.99.1 'echo | /home/root/xovi/rebuild_hashtable'
```

Then start XOVI:

```sh
ssh root@10.11.99.1 'systemd-run --collect --unit=xovi-restart bash /home/root/xovi/start'
```

Before going further, confirm that the AppLoad launcher opens on the tablet.

XOVI doesn't load by itself after a reboot. If the AppLoad icon disappears after a restart, press the power button three times within two seconds.

## 6. Install the apps

On your computer:

```sh
curl -LO https://github.com/keithwithAI/remarkable-move-apps/releases/latest/download/secondbrain-device.tar.gz
tar xzf secondbrain-device.tar.gz
scp -r secondbrain root@10.11.99.1:/home/root/
ssh root@10.11.99.1 'sh /home/root/secondbrain/deploy/install.sh'
```

The installer copies the binaries to `/home/root/secondbrain/bin`, sets up the timers, adds the six apps to AppLoad, writes a starting config file if there isn't one, and turns off automatic OS updates. It's safe to run again.

Restart XOVI so the launcher picks up the new apps:

```sh
ssh root@10.11.99.1 'systemd-run --collect --unit=xovi-restart bash /home/root/xovi/start'
```

## 7. Fill in the config

The config file is `/home/root/secondbrain/config/config.toml`. Set up only the sections you want. Relative paths in it are resolved against the config folder.

**Google Calendar.** In the Google Cloud console, create a service account, enable the Calendar API and download a JSON key. Copy it to the tablet:

```sh
scp service-account.json root@10.11.99.1:/home/root/secondbrain/config/
```

Share each calendar with the service account's email address (`client_email` in the JSON file), then list the calendar IDs under `calendars` in the config.

**Todoist.** Copy your API token from Todoist under Settings > Integrations > Developer and save it on the tablet:

```sh
ssh root@10.11.99.1 'cat > /home/root/secondbrain/config/todoist.token'
```

Paste the token, press Return, then press Ctrl-D.

**Vault sync.** Follow [deploy/reader-sync-setup.md](secondbrain/deploy/reader-sync-setup.md). To try the Reader without Git, copy some Markdown files into `/home/root/secondbrain/vault/`. The repo includes a few in [secondbrain/examples/sample-vault](secondbrain/examples/sample-vault).

Re-run the installer when you're done, so it locks down the permissions on your credential files:

```sh
ssh root@10.11.99.1 'sh /home/root/secondbrain/deploy/install.sh'
```

## 8. Add the Kindle Browser runtime (optional)

The Kindle Browser needs a WebKit runtime, about 138 MB, which downloads once:

```sh
ssh root@10.11.99.1 'sh /home/root/secondbrain/deploy/wpe-setup.sh'
```

The browser opens Amazon's UK store by default. To use the US store:

```sh
ssh root@10.11.99.1 'mkdir -p /home/root/secondbrain/browser && echo https://read.amazon.com/kindle-library > /home/root/secondbrain/browser/default-url'
```

Only `read.amazon.com` and `read.amazon.co.uk` are accepted. The browser only opens Amazon's own sites, because its WebKit is a fixed copy with known security risks, described in the [README](README.md#risks). If a sign-in page won't load, look for `blocked navigation` in `/home/root/browser.log` and open an issue with the blocked address.

## 9. Check that it works

```sh
ssh root@10.11.99.1 'systemctl list-timers | grep -E "agenda|vaultsync"'
ssh root@10.11.99.1 '/home/root/secondbrain/bin/agenda --config /home/root/secondbrain/config/config.toml'
```

Open each app from the AppLoad launcher, then put the tablet to sleep and check the lock screen shows your agenda.

## After an OS update

An OS update removes this project's system services and can make AppLoad crash, so check XOVI and AppLoad support the new version first. Your files in `/home/root` are kept. After updating, rebuild XOVI's hash table as its documentation describes, then run:

```sh
ssh root@10.11.99.1 'sh /home/root/secondbrain/deploy/install.sh'
```

## SSH over WiFi (optional)

```sh
ssh root@10.11.99.1 'rm-ssh-over-wlan on'
```

This exposes root login to your local network, and password login stays enabled. Turn it off with `rm-ssh-over-wlan off`.

## Removing everything

To go back to stock software, use reMarkable's [recovery tool](https://support.remarkable.com/s/article/Software-recovery). reMarkable documents this as the way to leave developer mode, so don't rely on a factory reset from Settings. The reset erases the tablet, so back up first.

Before selling or giving away a tablet, also revoke the credentials it held: the Google service account key, the Todoist token and any deploy keys.
