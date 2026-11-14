# Splat Live : application de bureau (Tauri 2)

Ce dossier contient la coquille de bureau. Elle fait trois choses :

1. elle démarre le serveur `gs-server` dans le même processus (pipeline de
   capture, HTTP local sur `127.0.0.1:7878`, HTTPS réseau à la demande) ;
2. elle ouvre la fenêtre « Splat Live » sur l'interface `gs-app` embarquée ;
3. elle expose une commande Tauri, `server_info`, qui renvoie le même JSON
   que `GET /api/info`.

L'interface parle ensuite au serveur comme le ferait un navigateur :
`http://127.0.0.1:<port>` pour l'API, `ws://127.0.0.1:<port>/ws` pour le flux.
Le certificat auto-signé de l'accès réseau est conservé dans le dossier de
données de l'application (`~/.local/share/dev.splatlive.desktop` sous Linux),
pour que le casque continue de lui faire confiance d'un lancement à l'autre.

## Prérequis

- Rust stable avec la cible `wasm32-unknown-unknown`, et [Trunk](https://trunkrs.dev) ;
- les bibliothèques système de Tauri 2 (sous Linux : WebKitGTK 4.1, voir la
  [documentation de Tauri](https://tauri.app/start/prerequisites/)) ;
- en option, la ligne de commande Tauri : `cargo install tauri-cli` (commande
  `cargo tauri`) ou le paquet npm `@tauri-apps/cli` (commande `tauri`).

Ce paquet fait partie de l'espace de travail Cargo mais pas de ses membres par
défaut : `cargo build` et `cargo test` à la racine ne le compilent pas et ne
demandent donc ni WebKitGTK ni interface déjà construite. Il se compile à la
demande avec `-p splat-live`.

## Développement

Avec la ligne de commande Tauri, depuis `src-tauri/` :

```bash
cargo tauri dev
```

Cette commande lance `trunk serve` dans `crates/gs-app` (port 8100, avec
rechargement à chaud) puis l'application, dont la fenêtre affiche ce serveur
de développement. Le serveur `gs-server` embarqué accepte alors l'origine
`http://127.0.0.1:8100` en plus des siennes.

Sans la ligne de commande Tauri, la même chose à la main, depuis la racine :

```bash
(cd crates/gs-app && trunk serve --dist ../../target/trunk-dev-dist) &
cargo run -p splat-live
```

À savoir : un binaire compilé sans la fonctionnalité `custom-protocol` est un
binaire de développement. Il charge `http://127.0.0.1:8100` et affiche une
erreur de connexion si `trunk serve` ne tourne pas.

## Version finale

Avec la ligne de commande Tauri, depuis `src-tauri/` :

```bash
cargo tauri build --no-bundle
```

Elle construit d'abord l'interface (`trunk build --release` dans
`crates/gs-app`), puis le binaire `target/release/splat-live` avec
l'interface embarquée. Sans `--no-bundle`, il faut d'abord passer
`bundle.active` à `true` dans `tauri.conf.json` pour obtenir les paquets
d'installation.

Sans la ligne de commande Tauri, depuis la racine :

```bash
(cd crates/gs-app && trunk build --release)
cargo build --release -p splat-live --features custom-protocol
./target/release/splat-live
```

La fonctionnalité `custom-protocol` fait deux choses : la fenêtre charge
l'interface embarquée (`tauri://localhost`) au lieu du serveur de
développement, et `gs-server` embarque sa propre copie de l'interface pour la
servir aux casques du réseau local. Les deux lisent `crates/gs-app/dist` à la
compilation : ce dossier doit exister.

## Variables d'environnement

| Variable | Effet |
|---|---|
| `SPLAT_LIVE_VERBOSE` | si elle est définie, journal détaillé sur la sortie d'erreur (connexions, requêtes HTTP) |

Ctrl-C ou un signal de terminaison ferme l'application proprement, comme la
fermeture de la fenêtre : les clients reçoivent une fermeture WebSocket et le
fil du pipeline est arrêté.

## Sécurité du contenu (CSP)

La politique déclarée dans `tauri.conf.json` autorise exactement ce dont
l'interface a besoin :

- `script-src 'self' 'wasm-unsafe-eval'` : le code WebAssembly de l'interface
  et du worker ;
- `worker-src 'self' blob:` : le worker du pipeline local ;
- `connect-src` avec `http://127.0.0.1:*` et `ws://127.0.0.1:*` (et
  `localhost`) : l'API et le flux du serveur embarqué, plus `ipc:` pour les
  commandes Tauri ;
- `img-src` avec `http://127.0.0.1:*` : le QR code servi par `/api/qr.svg`.

En mode développement, Tauri n'applique aucune CSP : une erreur de politique
ne se voit que dans un binaire construit avec `custom-protocol`.

## Essai sans l'interface réelle

`e2e/server/tauri-smoke.sh` compile la coquille avec une page de test
(`e2e/server/standin`) à la place de l'interface, la lance sous Xvfb et
vérifie dans le journal et sur une capture d'écran que la fenêtre affiche la
page, que la commande `server_info` répond et que les trames WebSocket
arrivent :

```bash
e2e/server/tauri-smoke.sh            # binaire de débogage
e2e/server/tauri-smoke.sh --release  # binaire optimisé
```

À noter : WebKitGTK n'expose pas WebXR. La réalité virtuelle passe par le
navigateur du casque, qui ouvre l'adresse HTTPS affichée dans l'application
(ou son QR code).
