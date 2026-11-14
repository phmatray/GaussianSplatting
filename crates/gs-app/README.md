# gs-app: l'interface de Splat Live

Application web monopage en Rust (Leptos 0.8, rendu côté client) construite
avec Trunk. Elle affiche les splats dans un `gs_render::Viewer` plein écran et
pilote le pipeline de capture, où qu'il tourne.

## Construire et lancer

```
cd crates/gs-app
trunk build --release        # produit crates/gs-app/dist
trunk serve                  # développement, http://127.0.0.1:8100
```

Le même dossier `dist` sert dans trois situations, sans modification (toutes
les adresses du build sont relatives):

| Situation | Source des splats |
|---|---|
| Dans la coque Tauri (`src-tauri`) | Serveur natif, trouvé par la commande `server_info` |
| Servi par `gs-server` à la racine | Serveur natif, trouvé par `GET /api/info` |
| Hébergement statique, y compris dans un sous-dossier | Worker local `splat-worker` (WebAssembly) |

La source active est affichée dans la barre du haut: « Serveur natif » ou
« Local (WASM) », avec l'état de la connexion. La liaison WebSocket se
rétablit toute seule, avec un délai croissant jusqu'à 5 secondes.

Un `cargo build` à la racine du dépôt compile ce paquet en simple coquille
(l'interface n'existe que pour `wasm32-unknown-unknown`); `cargo test -p gs-app`
exécute les tests de `src/logic`, qui n'ont pas besoin de navigateur.

## Paramètres d'adresse

| Paramètre | Effet |
|---|---|
| `?source=worker` | Force le worker local, sans chercher de serveur |
| `?source=server` | Force le serveur natif; s'il ne répond pas, l'interface le dit et réessaie |
| `?scene=house` | Avec le worker: démarre directement sur le scan de maison |

## Raccourcis

| Touche | Effet |
|---|---|
| H | Masque ou réaffiche l'interface |
| P | Pause, reprise |
| Échap | Réaffiche l'interface |
| ZQSD ou WASD, flèches | Vol libre (touches physiques) |
| E ou Espace, A (Q) | Monter, descendre |
| Maj | Aller plus vite |

Les touches de vol sont ignorées pendant la saisie dans un champ.

## Organisation du code

| Module | Rôle |
|---|---|
| `src/logic` | Tout ce qui se calcule sans navigateur: choix de la source, correctifs de configuration, présentation de la scène (repères, vue maquette, cadrage), couleurs de profondeur, formats de fichiers, mise en forme des nombres |
| `src/core.rs` | Le viseur, le transport et le traitement des messages |
| `src/transport` | `Transport::Server` (WebSocket) et `Transport::Worker` (Web Worker): mêmes octets, mêmes commandes |
| `src/state.rs` | Les signaux réactifs lus par les vues |
| `src/ui` | Barre du haut, panneau latéral, éléments superposés |
| `src/debug.rs` | `window.__splatLive` pour les vérifications automatiques |
| `worker-boot.js` | Chargeur du worker, versionné par l'identifiant de build |

La configuration affichée est toujours celle que le pipeline renvoie dans son
message `State`: un réglage modifié par un autre client apparaît donc aussi.
Les modifications partent en `patch_config`, regroupées après un court délai.

## Point d'accès pour les tests

`window.__splatLive.state()` renvoie un objet JSON: `source`, `link`,
`connected`, `scene`, `config`, `layers`, `viewer`, `nav`, `eye`, `target`,
`display`, `xr`, `stats`, `counts`, `previewCount`, `previews`, `scan`, `file`,
`fileMode`, `fileError`, `notice`, `transferBytesPerSecond`, `server`,
`panelOpen`, `uiHidden`, `errors`. `window.__splatLive.viewerEye()` renvoie la
position de la caméra `[x, y, z]`.

Les vérifications de bout en bout sont dans `e2e/app` (`node e2e/app/run.cjs`).
