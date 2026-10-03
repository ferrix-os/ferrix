# Ferrix discovery plan

Updated 2026-09-27. Ferrix is experimental software. Lead with what a visitor
can see and try, then explain how it was built.

The reusable logo, colours and short descriptions are in the
[press and brand kit](README.md).

## The story

**Hook:** Linux apps without Linux.

**Plain description:** Ferrix is a Rust operating system with its own kernel.
It runs tested, unmodified Linux programs, including Chrome, `rustc`, `git`
and `curl`. Its disk, network, graphics and input drivers run as separate
processes that can restart after a crash.

Say *tested Linux programs*, rather than implying all Linux software works.
Say *builds and boots its own x86-64 image*, rather than claiming complete
self-hosting. The AI-agent development process belongs in the project's story
and documentation; it is not the product headline.

## What is already in place

- The website has a descriptive title and summary, canonical URL, social
  preview tags, structured data and an XML sitemap. GitHub Pages serves it at
  <https://ferrix-os.github.io/>.
- The repository has a homepage, description, focused topics, README,
  contribution guide, issue forms and Discussions. The live description and
  topics were refreshed on 2026-09-27; `tools/common/release/github-setup.sh`
  records them for future setup.
- `docs/brand/social-preview.png` is 1280×640 and under 1 MB. Upload it under
  the repository's **Settings → General → Social preview** if it is not there.
  This setting is separate from the website's Open Graph image.
- `release.yml` can publish GitHub Releases when a tagged release is ready.
  There is no published latest release yet. Do not backfill old stage tags just
  to manufacture activity.

## What to do next, in order

1. **Make the first visit easy.** Keep the README's first screen to the hook,
   what runs, a candid limitation and a command that boots Ferrix. Keep a real
   screenshot nearby. Check the command on a clean machine before a launch.
2. **Keep GitHub's About panel current.** Its description, homepage and topics
   now match the site. Review them after major milestones. Pin Ferrix on the
   owner's profile. GitHub lets people browse and search repositories by topic.
3. **Publish a useful release.** A release should have a specific improvement,
   a screenshot or short demo, the boot command, and known limitations. Attach
   a bootable image when it is practical and tested. People can choose GitHub
   notifications specifically for releases.
4. **Create a short demo.** Show Ferrix booting, the desktop, one Linux app
   running unchanged, and `rustc` compiling a small program. Link the demo
   from the site and README. Use real captures and label host acceleration.
5. **Publish technical explanations.** Good topics: how Ferrix runs Linux
   binaries, restarting a userland driver, and what the self-build test
   actually proves. Each page needs a concrete diagram, log, command or
   source link. Link to it from the relevant website section and README.
6. **Share a milestone where its readers are.** A Rust or OS development
   community will care about kernel design and proof. A general programming
   community will care about the unmodified programs and the short demo. Write
   a fresh post for each audience and answer technical questions. Do not ask
   people to star the repository just to influence a ranking.

## Search visibility

The current site is one page. Its title says *Rust operating system* and
*Linux apps*, while the visible headline stays short. The copy uses those
terms where they help explain the project, without repeating keyword lists.

Google's [SEO starter guide](https://developers.google.com/search/docs/fundamentals/seo-starter-guide)
recommends clear titles, useful content, crawlable pages and descriptive
links. It says there is no way to guarantee a first-place ranking. The
[sitemap guide](https://developers.google.com/search/docs/crawling-indexing/sitemaps/build-sitemap)
calls for absolute canonical URLs; it does not promise indexing. The website
already has a sitemap at <https://ferrix-os.github.io/sitemap.xml>.

After deployment, add the GitHub Pages URL to Google Search Console and Bing
Webmaster Tools, verify ownership and submit the sitemap. The owner must do
this in those accounts. Review the index coverage and search queries monthly.
If Ferrix gets a custom domain, update the canonical URL, sitemap, social URLs
and GitHub homepage together. Keep the old GitHub Pages address reachable.

Do not count on FAQ markup for a search feature: Google [restricts FAQ rich
results](https://developers.google.com/search/blog/2023/08/howto-faq-changes)
mainly to authoritative government and health sites. Keep the FAQ because it
answers real questions.

## GitHub discovery

There is no documented switch for getting Ferrix “featured more often.”
GitHub documents several practical discovery surfaces:

- [Topics](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/classifying-your-repository-with-topics)
  make a repository browsable by subject and searchable with topic filters.
- [Stars](https://docs.github.com/en/get-started/exploring-projects-on-github/saving-repositories-with-stars)
  help people save and discover repositories; GitHub says Explore shows
  popular repositories partly by star count.
- [Releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases)
  can notify people who chose release notifications. A tag alone is not a
  substitute for a release with useful notes and an artifact.
- A [repository social preview](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/customizing-your-repositorys-social-media-preview)
  improves how repository links appear when shared elsewhere.

GitHub does not publish a precise Trending ranking formula. Treat Trending
as a possible result of a strong launch, not a goal that can be scheduled or
guaranteed. Do not claim that a certain number of stars in 24 hours triggers
it. Better measures are qualified visitors, successful boots by people outside
the project, useful issues and repeat contributors.

## Measure what changes

Check monthly: Search Console impressions and clicks, GitHub's 14-day traffic
views and referrers, release asset downloads, and issues from first-time
users. Record which posts brought people who actually tried Ferrix. Avoid
using star count alone as the project's success metric.
