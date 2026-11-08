# Reader ↔ Obsidian highlight sync (one-time Mac setup)

The Reader writes Obsidian `==highlights==` straight into the clipping `.md` on the device. To get
them back into Obsidian on your Mac, the device's vault and your Obsidian vault share one Git repo:

    Mac (Obsidian Git) ──push──▶ GitHub ◀──pull/push── device (vaultsync, every 20 min)

## Steps

1. **Make the clippings a Git repo.** Easiest: open your clippings folder as its own Obsidian
   vault. Create a **private** GitHub repo and push the folder to it. (You can also use a whole
   vault, see step 5.)

2. **Install the Obsidian Git plugin** (Community plugins → “Git”). Enable:
   - *Auto commit-and-sync* every ~10 min, and
   - *Auto pull* on startup / interval.

   This pushes your edits + new clippings and pulls the device's highlights.

3. **Give the device a deploy key.** The device only has `dropbearkey`, whose keys libgit2 can't
   read, so generate the key on your computer and copy it over. The private key stays on the
   device and is never committed:

       ssh-keygen -t ed25519 -N "" -f rmpp_deploy_key
       scp rmpp_deploy_key root@10.11.99.1:/home/root/secondbrain/config/id_ed25519
       ssh root@10.11.99.1 'chmod 600 /home/root/secondbrain/config/id_ed25519'

   Add `rmpp_deploy_key.pub` to the GitHub repo under Settings > Deploy keys, with
   **Allow write access** on. Then give the device GitHub's host keys, which libgit2 checks:

       ssh-keyscan github.com | ssh root@10.11.99.1 'mkdir -p /home/root/.ssh && cat >> /home/root/.ssh/known_hosts'

4. **Point the device at the repo** — in `/home/root/secondbrain/config/config.toml`:

       [vault]
       repo_url = "git@github.com:<you>/<repo>.git"
       local    = "/home/root/secondbrain/vault"
       branch   = "main"
       ssh_key  = "/home/root/secondbrain/config/id_ed25519"

   Then clone once: `ssh root@10.11.99.1 '/home/root/secondbrain/bin/vaultsync'` (or wait for the timer).

5. **Reader folder.** The Reader app reads `--dir /home/root/secondbrain/vault` (set in the AppLoad
   manifest). If your repo is the *whole* vault rather than just the Clippings folder, change it to the
   subfolder, e.g. `--dir "/home/root/secondbrain/vault/Clippings"`, and re-run
   `deploy/install.sh` on the device.

## The loop

Read a clipping → pen-drag a passage → the Reader writes `==…==` and nudges `vaultsync` → the push
lands on GitHub → Obsidian Git pulls it → the highlight appears in Obsidian, byte-identical.
