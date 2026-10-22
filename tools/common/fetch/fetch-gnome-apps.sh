#!/usr/bin/env bash
# Fetch GTK 4, libadwaita and their demonstration apps from Debian 13, with
# Mesa and a session bus, and put them on a btrfs volume of their own: the
# disk `cargo xtask test-gnome-apps` runs them from as clients of hyprix
# (docs/GNOME.md, stage G1).
#
# Nothing here is built and nothing of Debian's runs on this machine but
# two cache writers named below. The tree is the dependency closure
# (Depends and Pre-Depends, never Recommends) of the packages in ROOTS,
# resolved once against trixie's and trixie-security's Packages files as
# snapshot.debian.org kept them at SNAPSHOT_DEBIAN and SNAPSHOT_SECURITY;
# every package is pinned below by its pool path and SHA-256 and downloaded
# from that snapshot, so the same script makes the same tree on any day.
# Only the packages' files are unpacked (`dpkg-deb -x`): no maintainer
# script runs, no apt, no dpkg database (the certification consultant's G1
# condition 1, ledger line 3100).
#
# What G1 forbids is refused here, not trusted to the list (condition 2):
#   * a package on DENY (the system bus, logind, polkit, accountsservice,
#     GDM, PAM, gnome-keyring, udisks2) stops the script;
#   * any file with a set-uid or set-gid bit, or a
#     `security.capability` attribute, stops the script, and so does a
#     maintainer script that would have set one (`setcap`,
#     `dpkg-statoverride`, a `chmod` to a set-id mode): those scripts never
#     run, but a package that wants either is not one G1 takes.
# The volume is mounted `nosuid,nodev` on Ferrix as well (the gate's
# `data-nosuid.service`).
#
# Three caches the packages' triggers would write are written here, by
# the tree's own programs run on its own loader (GLib's schema compiler is
# this machine's): GLib's compiled GSettings schemas, gdk-pixbuf's list of
# image loaders (with the tree's path taken out, so it names them where
# Ferrix's root links them, /usr/lib/x86_64-linux-gnu), and the MIME
# database. After unpacking, every library each ELF
# file in the tree needs is looked for in it, so a dependency this list
# lacks fails here rather than on Ferrix.
#
# Writes $FERRIX_GNOME_APPS_VOLUME/tree, gnome-apps.img beside it and,
# once both are whole, gnome-apps.stamp (default
# ~/.local/share/ferrix/gnome-apps). Needs curl, sha256sum, dpkg-deb,
# readelf, python3 and mkfs.btrfs; no root.
#
# Usage: tools/common/fetch/fetch-gnome-apps.sh

set -euo pipefail

snapshot=${DEBIAN_SNAPSHOT:-https://snapshot.debian.org/archive}
SNAPSHOT_DEBIAN=20261019T203124Z
SNAPSHOT_SECURITY=20261019T110620Z
data=${XDG_DATA_HOME:-$HOME/.local/share}/ferrix
out=${FERRIX_GNOME_APPS_VOLUME:-$data/gnome-apps}
pool=${FERRIX_GNOME_APPS_POOL:-$out/pool}

# What the closure was resolved from, on 2026-10-22: GTK 4 and libadwaita,
# their demos, Mesa's EGL, GLES, gbm and Gallium drivers (virgl), the
# session bus and its configuration, fonts, icons and schemas. Left out of
# the closure as install-time only: debconf, init-system-helpers, and the
# system bus's packages (default-dbus-session-bus names dbus-daemon,
# which is here).
ROOTS="libgtk-4-1 libadwaita-1-0 gtk-4-examples libadwaita-1-examples libegl-mesa0
libgl1-mesa-dri libgles2 libegl1 libgbm1 dbus-daemon dbus-bin dbus-session-bus-common
fontconfig-config fonts-cantarell adwaita-icon-theme gsettings-desktop-schemas libgtk-4-bin
librsvg2-common"

# Packages G1 may not bring (ledger line 3100, condition 2): matched
# against each pinned package's name.
DENY='^(dbus|dbus-system-bus-common|dbus-user-session|systemd|systemd-sysv|libpam-systemd|libpam-.*|libpam[0-9].*|policykit-1|polkitd|pkexec|gvfs.*|accountsservice|gdm3|gnome-keyring|udisks2|xdg-desktop-portal.*)$'

# Packages a pinned package may depend on that are not pinned: what only
# their maintainer scripts use, which never run here. Anything else a
# pinned package needs and the list lacks stops the script.
INSTALL_ONLY='^(debconf|debconf-2\.0|init-system-helpers|default-dbus-session-bus|dbus-session-bus)$'

# The system bus's set-uid activation helper, which is not in dbus-daemon
# 1.16.2's package (dpkg-deb -c, 2026-10-22) but in the system bus's, which
# are denied above. Its absence is asserted, in every archive's listing and
# in the tree.
LAUNCH_HELPER=dbus-daemon-launch-helper

# Archive, pool path and SHA-256 of each package, from the Packages files
# at the snapshots above.
debs=(
    "main pool/main/a/adwaita-icon-theme/adwaita-icon-theme_48.1-1_all.deb f60b0577aee314d43d6908ca7f0b8f07dc48e2bb766e7e25035330817e0e827c"
    "main pool/main/d/dbus/dbus-bin_1.16.2-2_amd64.deb a7c0785f89d3763bb83e2912237135a7d1c610e06f487c39b1d6408e88d46fb5"
    "main pool/main/d/dbus/dbus-daemon_1.16.2-2_amd64.deb c91558cbd019bd5ec3e9fde0084976d5f61977f4ef0a9ed1ff48fe3a129b91d4"
    "main pool/main/d/dbus/dbus-session-bus-common_1.16.2-2_all.deb 98bd27836c7170839ed9c84b00daad9ed7155bd3d42e3f0ec3faf497aafa5f5f"
    "main pool/main/d/dconf/dconf-gsettings-backend_0.40.0-5_amd64.deb 393e8deffcc78d4396e860f10cb5e7d0aab448d6e0965add62941fcd27be00fb"
    "main pool/main/d/dconf/dconf-service_0.40.0-5_amd64.deb 4a1feeda4605be8561ec32fb4d0655fbe2313e62e457107168f528d959b530b8"
    "main pool/main/f/fontconfig/fontconfig_2.15.0-2.3_amd64.deb 09794b188e042c90b4a89fcb5afeb276bc231b74ddf8a3efbbc7dfb6f0eac4cb"
    "main pool/main/f/fontconfig/fontconfig-config_2.15.0-2.3_amd64.deb 0475c00d02660c07a15085051818625331bb502e053242106aaaa1f2ddb41225"
    "main pool/main/f/fonts-cantarell/fonts-cantarell_0.303.1-4_all.deb 21f8160696cd2b51ac104e735fd63d0e2245cd84cf75f17919f1425dce4ea258"
    "main pool/main/f/fonts-dejavu/fonts-dejavu-core_2.37-8_all.deb 86635b3d25b3655fc11cb3ecc3af59f0bf19643b02b94f2de48bd10253cdba12"
    "main pool/main/f/fonts-dejavu/fonts-dejavu-mono_2.37-8_all.deb 3003e98a5debfdeadc7040a7f715fe9fe6fb67f68deacf6049b54e30f07fc014"
    "main pool/main/g/gcc-14/gcc-14-base_14.2.0-19_amd64.deb 5b6825de4263824b78c4c51f6476414f3b4e89c2ab63e81dc8b9b5501e867cf6"
    "main pool/main/g/gsettings-desktop-schemas/gsettings-desktop-schemas_48.0-1_all.deb 151133be02bff0bff4c860260a3418378821fad8c5d11133f588d4de760797a5"
    "main pool/main/g/gtk4/gtk-4-examples_4.18.6+ds-2_amd64.deb f488adb7a59e489951068cb969b593ec721f2a48e8bdef2941163edd3b166a2e"
    "main pool/main/g/gtk4/gtk-update-icon-cache_4.18.6+ds-2_amd64.deb 88378a48ac07d253fb9bf0503dfbfbee2037ed9cd83eeff1df398801ab183125"
    "main pool/main/h/hicolor-icon-theme/hicolor-icon-theme_0.18-2_all.deb d91fef83a186d3fc9fff293bfd7da4f684f736d15459d258a5a0d251168c1a35"
    "main pool/main/liba/libadwaita-1/libadwaita-1-0_1.7.6-1~deb13u1_amd64.deb b3ff133512acc2d303892eba6eeac29506efd579e418fdbce6af6dcb50b3de4c"
    "main pool/main/liba/libadwaita-1/libadwaita-1-examples_1.7.6-1~deb13u1_amd64.deb e150a295d0f23f39fa7fc59e48a213f5260f97c40435cee0fc4297cc214d77d7"
    "main pool/main/a/apparmor/libapparmor1_4.1.0-1_amd64.deb 4158976c77038b93021967f425b4cc505df18c4f5e3f444732399e7ed4af4419"
    "main pool/main/a/appstream/libappstream5_1.0.5-1_amd64.deb 532cdb01d8b8bc522719bba46fa61d16f91a3efbad32394466b97d4a69340a7d"
    "main pool/main/g/gcc-14/libatomic1_14.2.0-19_amd64.deb 212b399aae2f7299203d261a57e49372e09565a9a5ea971905f94a3960366c05"
    "main pool/main/a/audit/libaudit-common_4.0.2-2+deb13u1_all.deb b9d32e5ff04fd747aacb7d6b3ebbbbf734ddefb2f1695f0b1b2a4b5e51b1052e"
    "main pool/main/a/audit/libaudit1_4.0.2-2+deb13u1_amd64.deb 3d1dd3f031a56f01b747e2acc0e14575212580e6fa63d800c44cb0a31f1edfd7"
    "main pool/main/a/avahi/libavahi-client3_0.8-16_amd64.deb 27df3f1c0190d13ec6d014c5515c2ec8e9a49d0983f48fc9e5756ee4e62d7e37"
    "main pool/main/a/avahi/libavahi-common-data_0.8-16_amd64.deb a3c0cb1326ed60cbbef133c31af28759f7a922d886d001a138d52bbf0af908d6"
    "main pool/main/a/avahi/libavahi-common3_0.8-16_amd64.deb fb092fc55f1c7f3db0ac64467f30ebae8145543570b8e99f17ce84cf1dd28bb6"
    "main pool/main/u/util-linux/libblkid1_2.41.5-0+deb13u1_amd64.deb 81535f3c2c0efc732965907c8749103a0a26377c761622c9ce39b4c92dcde52f"
    "main pool/main/b/brotli/libbrotli1_1.1.0-2+b7_amd64.deb 0fb79f88db210afbd69282ab9649e525f393ec6950ca34da1a6b359250b8d7db"
    "main pool/main/libb/libbsd/libbsd0_0.12.2-2_amd64.deb e5a85986fa6bec3307ab1bc860736b478b331882bc45e17675a7bdf88eecb43a"
    "main pool/main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb cba4cda04244b5e481bb15524bc3c983a7d1b6f330013b9b381706a2fcb65310"
    "main pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "main pool/main/c/cairo/libcairo-gobject2_1.18.4-1+b1_amd64.deb 4fcd432ebe8f93599bd05426f304e2f70409aad271fc6b4b3a2c6243c99c377c"
    "main pool/main/c/cairo/libcairo-script-interpreter2_1.18.4-1+b1_amd64.deb 52b45eb6e49c53c0a35ef45555a9457b094482eafef27d27cd777b916a2f06de"
    "main pool/main/c/cairo/libcairo2_1.18.4-1+b1_amd64.deb 01390b049661e4cc8bd39cef78b41c2587d4035619d3b00d6c7846f43d80bbba"
    "main pool/main/libc/libcap-ng/libcap-ng0_0.8.5-4+b1_amd64.deb 20a9e4b0619a3eb2566338223bed135dde5a601eaa2f8fdd5f20b94506addcac"
    "main pool/main/libc/libcap2/libcap2_2.75-10+deb13u1+b3_amd64.deb 89fc4d34fc7a28ad6f0fcd0c561ab253b9dedf6f77f5a000b47c276c8295bf67"
    "main pool/main/libc/libcloudproviders/libcloudproviders0_0.3.6-2_amd64.deb a626bb23ed9ebc4bc3ce446b552048de63d76768bf0fb492754965c5ee28a7e4"
    "main pool/main/c/colord/libcolord2_1.4.7-3_amd64.deb f11fd3b492781122edf0790f1e1fe24c51a41c67d4bb76cff84a9426c5654fa7"
    "main pool/main/e/e2fsprogs/libcom-err2_1.47.2-3+b12_amd64.deb bd43e020b8fed399db3eebf098111f10fba4c436fff99ccad72cc7e27c9764ec"
    "main pool/main/c/cups/libcups2t64_2.4.10-3+deb13u2_amd64.deb 278c57da32afceec1f406869e790b29f40c75b02bb8e512dd903d5a5264df9d2"
    "security pool/updates/main/c/curl/libcurl3t64-gnutls_8.14.1-2+deb13u7_amd64.deb 9475c591b338b81859ee6d06bf2a9fbb01b52f184c235afa7f6e7f1652136e55"
    "main pool/main/libd/libdatrie/libdatrie1_0.2.13-3+b1_amd64.deb 339eb88f23d72b8f0b7b1aebcefbab6de600d97b88bc6222dd8aad11d2ca9051"
    "main pool/main/d/dav1d/libdav1d7_1.5.1-1_amd64.deb 8c64de4465a8ae1ca23e1dada1dfc73d7b431540fd5ae18f21430928576d5b05"
    "main pool/main/d/db5.3/libdb5.3t64_5.3.28+dfsg2-9_amd64.deb 18d02510ee78b67e4504ba050176797a200ff24214a7cd318082ab60ad7bf3fc"
    "main pool/main/d/dbus/libdbus-1-3_1.16.2-2_amd64.deb bbcb711daff7e104b5f80f9b05475b6142674bfe65518a36a56e96a91068a3f5"
    "main pool/main/d/dconf/libdconf1_0.40.0-5_amd64.deb 6e7f670e1cf493ae8f60dea52f5a4aef56605d5b06d921113d7c2a2800ff5af1"
    "main pool/main/libd/libdeflate/libdeflate0_1.23-2_amd64.deb 5fe37fb79d53f5c91da6ba3ac4c1a8453e1f2b1ff0e0ea45189c5338bbfdbf82"
    "main pool/main/libd/libdrm/libdrm-amdgpu1_2.4.124-2_amd64.deb c1d97a5e32e2bc68e833b8abeae026b6306198e87ee4396a7ffecd4779caefbf"
    "main pool/main/libd/libdrm/libdrm-common_2.4.124-2_all.deb 9a8a6c65c165e9964f106fb4ac710959b5d33e0790227e3ab6b27c4742d1254a"
    "main pool/main/libd/libdrm/libdrm-intel1_2.4.124-2_amd64.deb 188ab1fd74c838b3c8055d62107351e16a4bb7a25d39c30bbb9aac30ddd37238"
    "main pool/main/libd/libdrm/libdrm2_2.4.124-2_amd64.deb fe2276901c7cd7b8079de63072d37fe1cbeb4eb001a3bc1f1d662ad89aa0890e"
    "main pool/main/libe/libedit/libedit2_3.1-20250104-1_amd64.deb b002ea172b9c1e34a67bc497c523c67bb74c3f0a4e98113cb083990a1f1d3bfe"
    "main pool/main/m/mesa/libegl-mesa0_25.0.7-2+deb13u1_amd64.deb 0ddebd7e91b048aac812611d6bd008343386cb140b51e5e66df74c6b6263be65"
    "main pool/main/libg/libglvnd/libegl1_1.7.0-1+b2_amd64.deb 62e7bc8ccd47a60e55aad17f83d8d474310d782eb8589bbce2ab714c31712406"
    "main pool/main/e/elfutils/libelf1t64_0.192-4_amd64.deb 94497b7e17b6f574a0605b380d454e20d3f01a9c63b70c2a2263f679d30053e1"
    "main pool/main/libe/libepoxy/libepoxy0_1.5.10-2_amd64.deb 4c4c8024f2175086de65bca9fdc3fbb967f2863ebf249d489c66d0c8103dd3a3"
    "security pool/updates/main/e/expat/libexpat1_2.9.0-1~deb13u1_amd64.deb feff874772cae904ae4d24e917b35664952bec4b4ae9100a4edda5a7fa746832"
    "main pool/main/libf/libffi/libffi8_3.4.8-2_amd64.deb 0ebdc340de33333639c3c63874cd4b15ac2e83dfa1ef3053b7eefaf4919f4f68"
    "main pool/main/f/fontconfig/libfontconfig1_2.15.0-2.3_amd64.deb 7ae91ec59857cc8e375eb2bdd371a4a69daea38f7787d06d474a847867d852b1"
    "main pool/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_amd64.deb e4947f3291528f03d574f2b01d5c5fc45c58c47480f32888a5cdf0f231ab584e"
    "main pool/main/f/fribidi/libfribidi0_1.0.16-1_amd64.deb f8c35ec8606d0843bd3cf020c6cfa39ddfe717ec58ed826b5855ba9927eb0d55"
    "main pool/main/m/mesa/libgbm1_25.0.7-2+deb13u1_amd64.deb 31fb6d76b9ceaf13848fa617df53f85f62626b4fe7464a93811c720af6d5f2dd"
    "main pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
    "main pool/main/g/gdk-pixbuf/libgdk-pixbuf-2.0-0_2.42.12+dfsg-4+deb13u1_amd64.deb 94150fb955b2d3a9a5e054e31c5a4b048bad4a479207e86cde46fbc975c838b4"
    "main pool/main/g/gdk-pixbuf/libgdk-pixbuf2.0-common_2.42.12+dfsg-4+deb13u1_all.deb 9f0c64e33cf5bb3226a5a6ddf630dd76530596e476e6b47a8b27da444ff43a03"
    "main pool/main/m/mesa/libgl1-mesa-dri_25.0.7-2+deb13u1_amd64.deb 722fad9944bdfd29eb6421576331a2697f83bc4b3551d693edb7aefdde6bb198"
    "main pool/main/libg/libglvnd/libgles2_1.7.0-1+b2_amd64.deb e12c18aa77939d122ee1dea520ba0440b97dc19200b7396e03dff03a3ece74c1"
    "main pool/main/g/glib2.0/libglib2.0-0t64_2.84.4-3~deb13u5_amd64.deb e2baf92c57d1db1753e5781a61450d9a01e7833c7bd2dfe87776a6811c11ecc2"
    "main pool/main/libg/libglvnd/libglvnd0_1.7.0-1+b2_amd64.deb 887f74008166549ce9e100c906aa937e95d6e5ce1c8d86efe8c95fd953359b9c"
    "main pool/main/g/gmp/libgmp10_6.3.0+dfsg-3_amd64.deb d0d0265eb01770f17afd0f7c8c0622f80479dcfbbe13653a0debeec61464e622"
    "main pool/main/g/gnutls28/libgnutls30t64_3.8.9-3+deb13u4_amd64.deb 18a8bdfd91c7e3bcb01719d55a2b56849c7160b34f1f52b1c4fdfdd41bf1352b"
    "main pool/main/g/graphene/libgraphene-1.0-0_1.10.8-5_amd64.deb ddcf98f17f6c0a7c76974d7238be1468e59c186034c1b806638d32d9f446ea6e"
    "main pool/main/g/graphite2/libgraphite2-3_1.3.14-2+deb13u1_amd64.deb 1769ba3ed3c1fc75b7fbe02678e32aa24391205813eceef4e1e3500015d12ee3"
    "main pool/main/k/krb5/libgssapi-krb5-2_1.21.3-5+deb13u1_amd64.deb 30847c1fde4240567d7ed3aeab4f655dd591203758b857e85e824045aae70299"
    "main pool/main/g/gtk4/libgtk-4-1_4.18.6+ds-2_amd64.deb 51ab1114a57702e703cce745c24caf5bd31ec1d2a900c72fd15fe47c6c030e68"
    "main pool/main/g/gtk4/libgtk-4-bin_4.18.6+ds-2_amd64.deb b81d6870d8a01fe04599c76073d0bf305f345884ccefd77faff9183a0dfb6af2"
    "main pool/main/g/gtk4/libgtk-4-common_4.18.6+ds-2_all.deb 32f0ad042f983b1d53cd2c260d4042eda9041b6debb71beb91937196b1ff3f6b"
    "main pool/main/h/harfbuzz/libharfbuzz-subset0_10.2.0-1+deb13u1_amd64.deb 2d0de8588408af221927b372c3c8d439d638a0a8abb549660ed5813ab529e6bc"
    "main pool/main/h/harfbuzz/libharfbuzz0b_10.2.0-1+deb13u1_amd64.deb 932165b933e5eb633293afe984a983ebe89cb27c923ce7208626f1da6c0c7941"
    "main pool/main/n/nettle/libhogweed6t64_3.10.1-1_amd64.deb b059ed155115ac09da295322de48ee1ed58ef5fa45edcc9e12ff0e94636ef25f"
    "main pool/main/libi/libidn2/libidn2-0_2.3.8-2_amd64.deb 90b039bcdc4578f8e1c4935adf8dbb525e36a164deefdbbb8c45bac347d48278"
    "main pool/main/j/jbigkit/libjbig0_2.1-6.1+b2_amd64.deb 95cbc44561651e1b3385454df08e8f2efe84498f6963a85259be2cc2fe798709"
    "main pool/main/libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_amd64.deb 5504299ad0e3c0a9ca9f962a68a650a040db52f9f416a109e74df48d55601d3f"
    "main pool/main/k/krb5/libk5crypto3_1.21.3-5+deb13u1_amd64.deb 7da07ee674b47f1f0be7cc89317c25310086a1f1761217d0f72e6ae2c5a69b84"
    "main pool/main/k/keyutils/libkeyutils1_1.6.3-6_amd64.deb 0b11ad17be0300b63ad4eeb4c6450fed24d34b7b740f23e5363dcb29ee6d5eba"
    "main pool/main/k/krb5/libkrb5-3_1.21.3-5+deb13u1_amd64.deb 47d71d6a7f2e59b9bae5f89602397594805113b95889ad18fa703cd53abafc97"
    "main pool/main/k/krb5/libkrb5support0_1.21.3-5+deb13u1_amd64.deb 3a0acd8b37955c0e102c756b52c97df2a31f67b96453c35dab70df218d309117"
    "main pool/main/l/lcms2/liblcms2-2_2.16-2+deb13u2_amd64.deb 32e53c2002ccea8c60c02779bea0ba49ac132fef702988a30d64af79b6617806"
    "main pool/main/o/openldap/libldap2_2.6.10+dfsg-1_amd64.deb 60069e4a550ca890113fa89d3001417781b18e146ec5d637d31e368dec9909bd"
    "main pool/main/l/lerc/liblerc4_4.0.0+ds-5_amd64.deb 2e0c13c0ddbde12968c2c713c2e0474a5b78e90d5216031256a725e8c76aa31c"
    "main pool/main/l/llvm-toolchain-19/libllvm19_19.1.7-3+b1_amd64.deb db0d614d61345ca710fb73e75105f1ec6e38898fa7b3aa99fa7eb7d8b0a11d57"
    "security pool/updates/main/x/xz-utils/liblzma5_5.8.1-1+deb13u2_amd64.deb b38acab30f295bf1066a6632e31cbd0a1980f2924b38040cda131d5dea5384ec"
    "main pool/main/l/lzo2/liblzo2-2_2.10-3+b1_amd64.deb f3032201fe2928a87e13f05ce256ff3ac2a7860c7e594dc51ae6d7348a4466ce"
    "main pool/main/libm/libmd/libmd0_1.1.0-2+b1_amd64.deb 7244ec3839b61fac0c1884fe08aaa040f26e8f1f35f1f5d3482eacefd30d1b44"
    "main pool/main/u/util-linux/libmount1_2.41.5-0+deb13u1_amd64.deb 6d00f45f2e80e078e906e3eedecd3ba6913e39fef49bff361ee583f17f00ec05"
    "main pool/main/n/ncurses/libncursesw6_6.5+20250216-2_amd64.deb 47baa2e11579583654583a066b69bd5a5d2f22423081f707ed6c0c3ba6d538b7"
    "main pool/main/n/nettle/libnettle8t64_3.10.1-1_amd64.deb 1b03d4a9cd9c8143ba50fe6396f36937e901a317d492d179f4596862c1731cfe"
    "main pool/main/n/nghttp2/libnghttp2-14_1.64.0-1.1+deb13u1_amd64.deb 896cb217537c09251fb909b8541349010ac279d802d871e98c06a62a2c67ce2c"
    "main pool/main/n/nghttp3/libnghttp3-9_1.8.0-1_amd64.deb 388e4c33b72829cbf0d098afdc8539fa5ba7458eddb077f5538fdb4d5538716b"
    "main pool/main/n/ngtcp2/libngtcp2-16_1.11.0-1+deb13u1_amd64.deb 627eec81ebbd48c4e6091f5cd9dc5070b792b7075000eed60ab08c7daa961caf"
    "main pool/main/n/ngtcp2/libngtcp2-crypto-gnutls8_1.11.0-1+deb13u1_amd64.deb 2a7f109c0c4db6a800e4661c5e5e34e1f1f83c8162482276183d1ada9da7c96c"
    "main pool/main/p/p11-kit/libp11-kit0_0.25.5-3_amd64.deb 784bf2063e166c8bc851a32623b74ebd85c499043a9d57c5bbd64fa63447f45a"
    "main pool/main/p/pango1.0/libpango-1.0-0_1.56.3-1_amd64.deb 4fa38472696d2ddb10bfc9a4692a9446e8478fdb1f48c769a609fc9fec3adfc5"
    "main pool/main/p/pango1.0/libpangocairo-1.0-0_1.56.3-1_amd64.deb 6994729c5b7a9649661ec078cf039991f22eff44600543aa917894f83c89cd05"
    "main pool/main/p/pango1.0/libpangoft2-1.0-0_1.56.3-1_amd64.deb b506cf66f78ddf542ddb4bb4a68b3d748d41caa6e190e4480430a6630c1e5105"
    "main pool/main/libp/libpciaccess/libpciaccess0_0.17-3+b3_amd64.deb d9a0091071635a84e837051e4813005ac445071731becea28fba4d9806df5252"
    "security pool/updates/main/p/pcre2/libpcre2-8-0_10.46-1~deb13u3_amd64.deb e226f661d918f04daf38cdbc4806b7ed7d6ef95c7eb0ade692fc350e31970040"
    "main pool/main/p/pixman/libpixman-1-0_0.44.0-3_amd64.deb 3862c87596f6b4b60fc916395fd69aa10af28a875a8e5c081a8cf30ddd120bd6"
    "security pool/updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u6_amd64.deb 0f8034d1c2547760430c81e8d87e5d6acd533a3493c80cf76dbecd0343b6773c"
    "main pool/main/p/procps/libproc2-0_4.0.4-9_amd64.deb c9d61caab1b2ddfc3f17edb95be31b9809fcb2daf27d853dff615bc4ebd08b4b"
    "main pool/main/libp/libpsl/libpsl5t64_0.21.2-1.1+b1_amd64.deb 59d42bb1f9ebc0d1776fe616efb08a7a8568b05982e00f70b03c63863db768ab"
    "security pool/updates/main/libr/librsvg/librsvg2-2_2.60.0+dfsg-1+deb13u1_amd64.deb 5f68f720eccefebda3855a30007cab9d7662042ccbc01c126aa5f697ae6e5791"
    "security pool/updates/main/libr/librsvg/librsvg2-common_2.60.0+dfsg-1+deb13u1_amd64.deb b653edb698c94157e7f3ace314751ac0709f82e8c2d9229e3b402d134e97c624"
    "main pool/main/r/rtmpdump/librtmp1_2.4+20151223.gitfa8646d.1-2+b5_amd64.deb 93baa2004cbe6c8721b9e81beed078612540aef120970b4751305c51c6697368"
    "main pool/main/c/cyrus-sasl2/libsasl2-2_2.1.28+dfsg1-9_amd64.deb 66e49d7ae026811b49a0050f18b0325960a2625ffd1ff60d91b7844a815fa9d2"
    "main pool/main/c/cyrus-sasl2/libsasl2-modules-db_2.1.28+dfsg1-9_amd64.deb a5c38659fbd62c33c6d9bfa2be43e75a572d1818531aad32dd74d0a58cc4bd19"
    "main pool/main/libs/libselinux/libselinux1_3.8.1-1_amd64.deb 68bb8d32bd8d6d7d2f5952a169db03d1484b46ae1e52abccdec42a19dccea5d5"
    "main pool/main/l/lm-sensors/libsensors-config_3.6.2-2_all.deb 3056da80c7d963af795dab480ab6f6f4b154ad4ac39f522dc52d17c834fea253"
    "main pool/main/l/lm-sensors/libsensors5_3.6.2-2_amd64.deb f0a994a6d7cfa695dea5343d0d1ba7eed796c0ad920c7282998b95f60049c4f6"
    "main pool/main/libw/libwebp/libsharpyuv0_1.5.0-0.1_amd64.deb 51934833d909588f7761fd84362114e90d8d2d38af57edf5df5c4a705e037547"
    "main pool/main/libs/libssh2/libssh2-1t64_1.11.1-1+deb13u2_amd64.deb dbb1024c192d4d292b7cfa902b96076cfe81b56a5eed4fb28da36e8bf543e49a"
    "security pool/updates/main/o/openssl/libssl3t64_3.5.7-1~deb13u3_amd64.deb ff16bc048bcd7d1b256094450b79c77947d8e76fe2a24bd99b91021d591fa074"
    "main pool/main/g/gcc-14/libstdc++6_14.2.0-19_amd64.deb ab1fa05837aa7a92aae748fd07a18a35f7d18bb4a71c4724fe2bbf0e32089de0"
    "main pool/main/s/snowball/libstemmer0d_2.2.0-4+b2_amd64.deb ee9510a4f48b6efad845cad80b8773d72219291d325e4e3c3d07b3930a7bf2ee"
    "main pool/main/s/systemd/libsystemd0_257.13-1~deb13u1_amd64.deb ab0d4127b5e46e6f8c015a1db15a62ba9ae274cdefa150083adf90ada0600ea1"
    "main pool/main/libt/libtasn1-6/libtasn1-6_4.20.0-2+deb13u1_amd64.deb 23fec6e06583ce2bad9b2c04c9b485e90440e259b1abf8677cd80d3ce60831ad"
    "main pool/main/libt/libthai/libthai-data_0.1.29-2_all.deb fd38d40602834d510a29140bd27fd48485105e834f03dccab1c02e2edaa794dd"
    "main pool/main/libt/libthai/libthai0_0.1.29-2+b1_amd64.deb e6364c07aeeffe9a95f267061d9df168e48e9dc9126c3b40e19dc829f67702a7"
    "main pool/main/t/tiff/libtiff6_4.7.0-3+deb13u3_amd64.deb 6bb2038f36817ae1ee6d33df03917b21835dcc2e0f3e3e80a71390e83c171173"
    "main pool/main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb 8b9f6a7983e9418564e48a627518de4c03917b56efe68d7f3e93bd8fffa1cc10"
    "main pool/main/s/systemd/libudev1_257.13-1~deb13u1_amd64.deb 5d41c284f5a93b05bc7d648b61a02dd2bb9ff05b2261ad1a8b7d96044a0cfa88"
    "main pool/main/libu/libunistring/libunistring5_1.3-2_amd64.deb 6dd3490bef06ea1096f32d10766b7e016cf579bfae8451d1b7df15ce05b8aa46"
    "main pool/main/v/vulkan-loader/libvulkan1_1.4.309.0-1_amd64.deb f47da79cd140264fe21cceb08bc87a71bc7fec05819e4a421f3f518d21101a37"
    "main pool/main/w/wayland/libwayland-client0_1.23.1-3_amd64.deb d1607e1db1a7c5378a2e5c6ae7b20f64dc6acf134f953d84a7f5204b74339f33"
    "main pool/main/w/wayland/libwayland-egl1_1.23.1-3_amd64.deb 7c5c3429028b39b9432792db08cb218c43891df439ebc648b83ebee676a80ec5"
    "main pool/main/w/wayland/libwayland-server0_1.23.1-3_amd64.deb 2967212bd582e0dffca443fdc44f4c660e7368d41f7ee3a7f6314e0c3abfe9ea"
    "main pool/main/libw/libwebp/libwebp7_1.5.0-0.1_amd64.deb c749744b04abdd1426b0c7718e516362c03a557c5151662092d6d4f2cef6e7c1"
    "main pool/main/libx/libx11/libx11-6_1.8.12-1_amd64.deb b5a3fd3bf8c8fd0364bfb9bea00dcba7fc301229bd02dded084632d31f5b0fb3"
    "main pool/main/libx/libx11/libx11-data_1.8.12-1_all.deb c54f87069888f80ba4da586da6147d74c7598ccdd8b90906dbc4271fa414c738"
    "main pool/main/libx/libx11/libx11-xcb1_1.8.12-1_amd64.deb e05f94d21a932fba5b09b9b13d99df776b155d3bc792c0e294451df9ffe1ba25"
    "main pool/main/libx/libxau/libxau6_1.0.11-1_amd64.deb 689a9f0e0ba3e2c65431f864871e303ee904de69dd28abfc462663fae030227f"
    "main pool/main/libx/libxcb/libxcb-dri3-0_1.17.0-2+b1_amd64.deb f446d42fb5fcebbb3e347368ba83616769fdb271d85b2f49048e337f3163d267"
    "main pool/main/libx/libxcb/libxcb-present0_1.17.0-2+b1_amd64.deb db95ea4630c55bd7f3281cb60ccf75b1627ef2f4399e1939f8f6161e584a92fb"
    "main pool/main/libx/libxcb/libxcb-randr0_1.17.0-2+b1_amd64.deb f5d9fe5fdf797918f81f0abf6cfd4270bd54659540cddb4da709ab7154523214"
    "main pool/main/libx/libxcb/libxcb-render0_1.17.0-2+b1_amd64.deb 7f6e36978e0f70a590fbad4042a692c70b50ca7cbcdc53fa7e9d9ab87933acc6"
    "main pool/main/libx/libxcb/libxcb-shm0_1.17.0-2+b1_amd64.deb d1777a813f484e89f3dcaadd1d5f2b1a13e9313b6bf70eded16d8ce743208fef"
    "main pool/main/libx/libxcb/libxcb-sync1_1.17.0-2+b1_amd64.deb 0ce4770ed1505be9ddc6473045e4662d062aff2f3077ee684265f01cfb543559"
    "main pool/main/libx/libxcb/libxcb-xfixes0_1.17.0-2+b1_amd64.deb f9c1aafc18bf9e4662e34bbaf3bb00f1f4e8c2fc1dd786fdfb6cb9cb6c64fae7"
    "main pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_amd64.deb 5c222a72d11b866447da31693254f738430726e3e065a384e82687b2fd2f978b"
    "main pool/main/libx/libxcursor/libxcursor1_1.2.3-1_amd64.deb bb0dbf58b804faaa74ed9efdae61c4b14bb0b4855680798f63e5ca6a4954976b"
    "main pool/main/libx/libxdamage/libxdamage1_1.1.6-1+b2_amd64.deb e51e43f23f3befbc1f9408271f4df6773d37caece9ce7e1f38abade382f7fbf7"
    "main pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_amd64.deb 0740dc760916b2008b45417a42a8fd7dd5de370fb57d31373f15034cda8acf0b"
    "main pool/main/libx/libxext/libxext6_1.3.4-1+b3_amd64.deb fc618ec40465e5ce48622606299cb47833efc3fb235ba15543b81f850722f443"
    "main pool/main/libx/libxfixes/libxfixes3_6.0.0-2+b4_amd64.deb 3fdd95d86d8e9d63e11f52070935c8f9f912c36aa3b17338d969f7685f3ed4fb"
    "main pool/main/libx/libxi/libxi6_1.8.2-1_amd64.deb 093d0903f35bb7a9f6815180ee040e6951fecf9b66c128cd72f064710210606e"
    "main pool/main/libx/libxinerama/libxinerama1_1.1.4-3+b4_amd64.deb 49d4e1628960407b8be2880a42428e7629801715a8a5697d42c0b0ca0a479911"
    "main pool/main/libx/libxkbcommon/libxkbcommon0_1.7.0-2_amd64.deb f75ee544f55acc6a271debfab3ea4ae0458afc89d81cfe1a71137e07d4895b86"
    "main pool/main/libx/libxml2/libxml2_2.12.7+dfsg+really2.9.14-2.1+deb13u3_amd64.deb e0c6b63ce4602a036a526f60fe5e6c1586710688058d98fc1001b9b3147b7efd"
    "main pool/main/libx/libxmlb/libxmlb2_0.3.22-1_amd64.deb 21288db65dd4b5b4e7f054674d383a5af328663d0cd81e6871e8bcde221b2f0e"
    "main pool/main/libx/libxrandr/libxrandr2_1.5.4-1+b3_amd64.deb 11e3490de93a8bbee3daba719cb8e1325a26fb3c125525c34bdcb7deb05eb9b2"
    "main pool/main/libx/libxrender/libxrender1_0.9.12-1_amd64.deb 9d042dfd5e613be1e02e6ddd0c5c4adef19c5eb08f6db838c2eba672c496dca4"
    "main pool/main/libx/libxshmfence/libxshmfence1_1.3.3-1_amd64.deb 7b339e9e5b2349723d35af4df89bcc7aa456bbdf8ba1754358f9b44c3fe1f964"
    "main pool/main/liby/libyaml/libyaml-0-2_0.2.5-2_amd64.deb 8a5475402801de445a17887bb72d03613565a6a84b216b4b606d1462c613eeb2"
    "main pool/main/z/z3/libz3-4_4.13.3-1_amd64.deb 71383373523ef62d47eccf660cf6535c3febcbb3f88e54cb7a43124014b57359"
    "main pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-1_amd64.deb 2f6a2aeacfc925eba8b00ac9139bc4bfccf8cacb09eb93de067074b26948eef9"
    "main pool/main/m/mesa/mesa-libgallium_25.0.7-2+deb13u1_amd64.deb 3e610f29321cdcc61337c86e6b4031ff60f0c9915fe19e6a862ac06480620ff4"
    "security pool/updates/main/o/openssl/openssl-provider-legacy_3.5.7-1~deb13u3_amd64.deb c12e0266c4780749a4702b8959ac979689f4182e18eed85a8a5eb90ed0e16eab"
    "main pool/main/p/procps/procps_4.0.4-9_amd64.deb 4db90bb6776772fc52585d6b1fbfc52ec2a85480e1b4363b743fbb2ffa2a5375"
    "main pool/main/s/shared-mime-info/shared-mime-info_2.4-5+b2_amd64.deb d7d061df29326b0ca6b273ac9332168659ff1562659f0a33609657a143dd7881"
    "main pool/main/x/xkeyboard-config/xkb-data_2.42-1_all.deb 196ff18533382f64e057ea49df2bb486bd4275a4cc0917361edb560b8756dada"
    "main pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb 015be740d6236ad114582dea500c1d907f29e16d6db00566ca32fb68d71ac90d"
)

say() { echo "fetch-gnome-apps: $*" >&2; }

for tool in curl sha256sum dpkg-deb readelf python3 mkfs.btrfs glib-compile-schemas; do
    command -v "$tool" > /dev/null || { say "$tool is not installed"; exit 1; }
done

# Download $2's $1 at its snapshot into the pool unless it is there, and
# check it against $3.
fetch() {
    local path=$1 archive=$2 sum=$3 base
    local file="$pool/${path##*/}"
    case $archive in
        main) base="$snapshot/debian/$SNAPSHOT_DEBIAN" ;;
        security) base="$snapshot/debian-security/$SNAPSHOT_SECURITY" ;;
        *) say "$path: no archive $archive"; exit 1 ;;
    esac
    if [ ! -f "$file" ]; then
        curl -fsSL --retry 3 -o "$file.part" "$base/$path" \
            || { say "${path##*/} could not be downloaded from $base"; rm -f "$file.part"; exit 1; }
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { say "$file does not match its pinned checksum $sum"; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$pool"
rm -f "$out/gnome-apps.stamp"

# The deny-list first, from the pins alone: nothing is downloaded for a
# list that names a package G1 may not bring.
for entry in "${debs[@]}"; do
    read -r _ path _ <<< "$entry"
    name=${path##*/}
    name=${name%%_*}
    if [[ $name =~ $DENY ]]; then
        say "$name is on the deny-list (docs/GNOME.md, G1): refused"
        exit 1
    fi
done

tree="$out/tree"
control="$out/control"
rm -rf "$tree" "$control"
mkdir -p "$tree" "$control"
fields="$out/fields"
: > "$fields"
for entry in "${debs[@]}"; do
    read -r archive path sum <<< "$entry"
    deb=$(fetch "$path" "$archive" "$sum")
    # The archive's own modes, before unpacking as a user can drop a set-id
    # bit and leave the tree's check below nothing to find.
    listing=$(dpkg-deb -c "$deb")
    if grep -q "$LAUNCH_HELPER" <<< "$listing"; then
        say "${path##*/} carries $LAUNCH_HELPER, which G1 refuses"
        exit 1
    fi
    setid=$(awk '$1 ~ /^-..[sS]|^-.....[sS]/ { print $NF }' <<< "$listing")
    if [ -n "$setid" ]; then
        say "${path##*/} has set-uid or set-gid files, which G1 refuses: $setid"
        exit 1
    fi
    dpkg-deb -x "$deb" "$tree"
    # The maintainer scripts, read and never run.
    name=${path##*/}
    dpkg-deb -e "$deb" "$control/${name%%_*}"
    dpkg-deb -f "$deb" Package Provides Pre-Depends Depends >> "$fields"
    echo >> "$fields"
done

# The list is the whole closure: every package's Depends and Pre-Depends
# met by a pinned package (or what one provides), or install-time only.
python3 -I - "$fields" "$INSTALL_ONLY" <<'PY' || exit 1
import re, sys
stanzas = [dict(re.findall(r'^([A-Za-z-]+): (.*)$', block, re.M))
           for block in open(sys.argv[1]).read().split('\n\n') if block.strip()]
have = set()
for st in stanzas:
    have.add(st['Package'])
    for p in st.get('Provides', '').split(','):
        if p.strip():
            have.add(p.strip().split(' ')[0])
skip = re.compile(sys.argv[2])
bad = 0
for st in stanzas:
    for clause in ','.join(st.get(k, '') for k in ('Pre-Depends', 'Depends')).split(','):
        alts = [re.sub(r'\s*\(.*?\)|:any', '', a).strip() for a in clause.split('|')]
        alts = [a for a in alts if a]
        if not alts or any(a in have or skip.match(a) for a in alts):
            continue
        print(f"fetch-gnome-apps: {st['Package']} needs {clause.strip()}, which is not pinned", file=sys.stderr)
        bad = 1
sys.exit(bad)
PY
rm -f "$fields"
wants=$(grep -lE 'setcap|dpkg-statoverride|chmod +[0-7]*[4-7][0-7]{3}|chmod +[ugoa]*\+[rwx]*s' \
    "$control"/*/{preinst,postinst} 2> /dev/null || true)
if [ -n "$wants" ]; then
    say "maintainer scripts that set a capability or a set-id bit, which G1 refuses:"
    printf '  %s\n' $wants >&2
    exit 1
fi
rm -rf "$control"

# Documentation and manuals, which nothing runs.
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/lintian" \
    "$tree/usr/share/bug"

lib="$tree/usr/lib/x86_64-linux-gnu"
loader="$tree/usr/lib64/ld-linux-x86-64.so.2"
test -x "$loader" || { say "no loader at usr/lib64"; exit 1; }
for program in usr/bin/adwaita-1-demo usr/bin/gtk4-demo usr/bin/gtk4-widget-factory \
    usr/bin/dbus-daemon usr/share/dbus-1/session.conf; do
    test -e "$tree/$program" || { say "no $program in the tree"; exit 1; }
done

# GSettings' schemas, compiled as the trigger would.
glib-compile-schemas "$tree/usr/share/glib-2.0/schemas"
test -f "$tree/usr/share/glib-2.0/schemas/gschemas.compiled" \
    || { say "the schemas did not compile"; exit 1; }

# gdk-pixbuf's loaders, listed by the tree's own query program on the
# tree's own loader, then named where Ferrix's root links them.
pixbuf="$lib/gdk-pixbuf-2.0/2.10.0"
"$loader" --library-path "$lib" "$lib/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders" \
    "$pixbuf"/loaders/*.so > "$pixbuf/loaders.cache.part"
sed -i "s|$tree/|/|g" "$pixbuf/loaders.cache.part"
grep -q '"svg"' "$pixbuf/loaders.cache.part" || { say "gdk-pixbuf lists no SVG loader"; exit 1; }
mv "$pixbuf/loaders.cache.part" "$pixbuf/loaders.cache"

# The MIME database GIO sniffs files with -- without it GTK takes no SVG
# for an image, and every symbolic icon is missing -- written by the
# tree's own update-mime-database, as shared-mime-info's trigger would.
XDG_DATA_DIRS="$tree/usr/share" "$loader" --library-path "$lib" "$tree/usr/bin/update-mime-database" \
    "$tree/usr/share/mime"
test -f "$tree/usr/share/mime/mime.cache" || { say "the MIME database was not written"; exit 1; }

# Every library an ELF file in the tree needs must be in it.
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        [ -e "$lib/$needed" ] || [ -e "$tree/usr/lib64/$needed" ] || [ -e "$(dirname "$file")/$needed" ] \
            && continue
        say "${file#"$tree"/} needs $needed, which is not in the tree"
        missing=1
    done
done < <(find "$tree/usr/bin" "$lib" -type f -print0)
[ "$missing" = 0 ] || exit 1

# No set-id file and no file capability in the tree as it goes on the
# volume (condition 2): checked last, after every cache was written.
setid=$(find "$tree" -xdev \( -perm /6000 -o -name "$LAUNCH_HELPER" \) -printf '%P\n')
if [ -n "$setid" ]; then
    say "set-uid or set-gid files in the tree, which G1 refuses:"
    printf '  %s\n' $setid >&2
    exit 1
fi
caps=$(python3 -I -c '
import errno, os, sys
def look(path):
    try:
        if "security.capability" in os.listxattr(path, follow_symlinks=False):
            print(path)
    except OSError as error:
        if error.errno in (errno.ENOTSUP, errno.EOPNOTSUPP):
            print("fetch-gnome-apps: " + path + ": this filesystem keeps no attributes; "
                  "the archive scan above stands alone", file=sys.stderr)
            return
        raise
look(sys.argv[1])
for top, dirs, files in os.walk(sys.argv[1]):
    for name in dirs + files:
        look(os.path.join(top, name))
' "$tree") || { say "the attribute check failed"; exit 1; }
if [ -n "$caps" ]; then
    say "files with a security.capability attribute, which G1 refuses:"
    printf '  %s\n' $caps >&2
    exit 1
fi

# Room for the session's caches and a size that does not depend on how mkfs
# rounds.
size=$(( $(du -sm "$tree" | cut -f1) + 256 ))
image="$out/gnome-apps.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
echo "${#debs[@]} packages, $SNAPSHOT_DEBIAN $SNAPSHOT_SECURITY" > "$out/gnome-apps.stamp"
echo "gnome-apps volume: $image (${size} MiB, ${#debs[@]} Debian 13 packages), unpacked in $tree"
