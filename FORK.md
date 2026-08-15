# What this fork is, and how to keep it

`heif-serialize` is a fork of [imazen/zenavif](https://github.com/imazen/zenavif)'s
serializer (itself descended from `mp4parse`), carried on `main` of
`felixbuenemann/heif-serialize` and consumed by SIS as a git submodule at
`image_engine/crates/heif-serialize`.

It path-depends on its sibling `heif-parse` as `../heif-parse`, so the two
directory names under `crates/` are load-bearing rather than cosmetic.

## Where this is going

Three futures, and the commit rule below serves all of them:

1. **A submodule for SIS.** What it is today.
2. **A standalone crate on crates.io.** `heif-parse` and `heif-serialize` are
   both unclaimed, checked 2026-08-11. This is a live option, and it is the one
   that most rewards the discipline: a published crate is judged on its
   history, its docs and whether it can be used without knowing who wrote it.
3. **Upstream.** Unlikely — see below.

## Why it diverged, and why upstreaming is unlikely

SIS is replacing libheif with these crates plus direct codec bindings. This one
grew the ability to WRITE what the parser learned to read: HEVC-coded primary
items, gain maps, alpha and grid tiles; timed image sequences with their loop
counts, clean apertures and auxiliary types; and colour stated on the track as
well as the item.

Upstream is an AVIF encoder/decoder project organised around AV1. A serializer
that emits `hvcC` and `hvc1` and HEVC-coded tracks is probably not what it
wants, and the crate rename makes the divergence explicit rather than
pretending otherwise.

Individual fixes might still be worth offering — the indefinite-duration fix for
repeating sequences in particular.

## The commit rule

**Every commit here should stand alone as a pull request to a project that has
never heard of SIS.**

- No SIS imports, SIS types, or SIS-specific naming. A reader who has never
  heard of SIS should not be able to tell what the change is for.
- Tests included, in the crate's own idiom, exercising the change on its own
  terms rather than through the SIS path that motivated it.
- A message that says what changed and why, with any measurement or file that
  prompted it named explicitly.
- `cargo fmt` scoped per package (`cargo fmt -p heif-serialize`) — never
  `--all`, which is banned across the consuming repo.

**Keep following it even if no PR is ever sent.** The rule was written to keep
the fork mergeable, but mergeability was never the whole value. What it
actually enforces is a commit that is about one thing, explains itself, and
carries its own test — which is what makes a change reviewable, a regression
bisectable, and a decision recoverable a year later.

It also keeps the crate honestly general, which is what a standalone release
depends on: a change that cannot be explained without mentioning SIS is usually
a change that belongs in the SIS crates that wrap this one, and the rule
catches that before the coupling is written. A crate published with SIS-shaped
seams in it is one nobody else can use.

## Divergence from upstream

**The crate is renamed.** Upstream it is `zenavif-serialize`; the rename is its
own commit. Read it as this crate's identity rather than as damage: `heif-serialize`
is what it would publish as, and the name says what it does. It is also the one
commit that could never be upstreamed, so it is kept isolated — which happens
to be exactly what makes it cheap to lift off if anyone ever does rebase onto
upstream instead.

Nothing else diverges. If something SIS-specific ever has to live here, record
it in this section with the reason, so it is visible later — and treat its
appearance as a sign the change belongs in the SIS crates that wrap this one.

## Building and testing

This repo is the crate, so the usual commands work directly:

```sh
cargo test -p heif-serialize
cargo fmt -p heif-serialize -- --check
```

`heif-parse` must be checked out beside it, since the dependency is a path.
