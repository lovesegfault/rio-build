<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
<!-- SPDX-FileCopyrightText: 2023 embr <git@liclac.eu> -->

Gorgon
======

Gorgon is an experimental CI system for your dependencies.

By tracking incoming changes to dependencies, and running your test suite against them,
it can potentially catch regressions that would've been missed by upstream, before they
make it into a release.

Note: This is a work in progress!


## Development Shell

Gorgon uses [`nix`](https://nixos.org/) to manage dependencies - all you need to do is
ask it for a development shell. All future commands in this file will assume you're in
one.

- If you have `direnv` set up, it can automatically set everything up when entering the
  project directory, and unset everything again when leaving it. This is the recommended
  approach:

  ```bash
  $ direnv allow
  ```

- If not, you can use `nix` to spawn a development shell:

  ```bash
  $ nix-shell --run $SHELL
  ```

## Give it a try!

In one terminal, start the supervisor for PostgreSQL by running:

```bash
$ process-compose
```

Then start `gorgond` by either:

- Selecting it in `process-compose` with the arrow keys and pressing F7, or
- Opening a second terminal and running:

  ```bash
  $ createdb gorgon
  $ cargo run -p gorgond -- -v run
  ```

These two do the exact same thing - launching it from `process-compose` just saves you
needing yet another terminal open.

Then open a new terminal, enter your dev shell, and create a project - a namespace for
things like builds:

```
$ cargo run -p gorgonctl -- project create test "Test Project"
{
  "project": {
    "id": "0196afee-7f42-77e2-a672-31d33fee39d9",
    "slug": "test",
    "name": "Test Project",
    "created_at": "2025-05-08T12:46:05.122387Z",
    "updated_at": "2025-05-08T12:46:05.122387Z"
  }
}
```

We'll manually create a build for now - in this case, we'll pull in a specific `nixpkgs`
commit, and try building Blender. This will be pretty quick, since Blender is already
available from the binary cache, but will still show off how this all works:

```
$ cargo run -p gorgonctl -- project builds test create -k nix \
    -i 'nixpkgs=git+https://github.com/NixOS/nixpkgs?rev=b75693fb46bfaf09e662d09ec076c5a162efa9f6' \
    -e '{ nixpkgs }: (import nixpkgs {}).pkgs.blender'
{
  "build": {
    "id": "0196afef-cf80-73e1-9f97-6391af3c2981",
    "project_id": "0196afee-7f42-77e2-a672-31d33fee39d9",
    "kind": "nix",
    "inputs": {
      "nixpkgs": "git+https://github.com/NixOS/nixpkgs?rev=b75693fb46bfaf09e662d09ec076c5a162efa9f6"
    },
    "expr": "{ nixpkgs }: (import nixpkgs {}).pkgs.blender",
    "worker_id": null,
    "result": null,
    "created_at": "2025-05-08T12:47:31.200392Z",
    "updated_at": "2025-05-08T12:47:31.200392Z",
    "allocated_at": null,
    "heartbeat_at": null,
    "started_at": null,
    "ended_at": null
  }
}
```

And then build it! _(Note that this may take a while.)_

```bash
$ cargo run -p gorgon-build -- -vvi $(hostname) --helper-dev
```

So how did that go? Let's find out!

```bash
$ cargo run -p gorgonctl -- project builds test list
```

You can also see a log of every event that happened during the build, though note that
it will be *very long* - here we're specifying `-c/--color` and `-f/--format` to get it
printed in human-friendly form, even though we're piping it to `less`:

```bash
$ cargo run -p gorgonctl -- build events 0196affc-8d33-7712-9a7c-61b3013fb73e -cf default | less
```

## Side Projects

Also in this repo is a few things that grew out of Gorgon, but can be used on their own.

- [nix-web](nix-web)
- [nix-supervisor](nix-supervisor)

## Contributing

Open pull requests at <https://codeberg.org/gorgon/gorgon/pulls>.

Please sign off your commits (`git commit -s`) to indicate your
certification of the [Developer Certificate of Origin](../DCO-1.1.txt).

---

[<img src="https://nlnet.nl/logo/banner.svg" width="200" alt="NLNet Foundation logo" />](https://nlnet.nl/)
[<img src="https://nlnet.nl/image/logos/NGI0Entrust_tag.svg" width="200" alt="NGI Zero Entrust logo" />](https://nlnet.nl/NGI0/)

This project was funded through the NGI0 Entrust Fund, a fund established by NLnet with financial support from the European Commission's Next Generation Internet programme, under the aegis of DG Communications Networks, Content and Technology under grant agreement Nº 101069594.
