# Vérifications de bout en bout de l'application

Playwright pilote la vraie application (`crates/gs-app/dist`) dans Chromium
sans affichage. Le rendu WebGL2 y est logiciel (SwiftShader): les images sont
justes, les cadences mesurées sont un plancher.

```
node e2e/app/run.cjs
```

Cette commande construit l'application (`trunk build --release` dans
`crates/gs-app`), la sert avec un petit serveur statique sur un port de la
plage 8100 à 8199, puis lance toutes les suites. Playwright doit être
installé sur la machine (il n'est pas installé d'ici); l'émulateur WebXR IWER
vient de `e2e/render` (`npm install` dans ce dossier, ou `GS_NPM_TOOLS`).

| Suite | Ce qu'elle vérifie |
|---|---|
| `stage` | Mode worker: source détectée, image non vide et animée, statistiques, vignettes RGB et profondeur, pause, nombre de caméras, résolution, bruit, ombres, calques, repères, orbite, contrôle de flux du worker, raccourcis H et P |
| `house` | Scan de maison: construction progressive, pause, « Terminer maintenant », vue maquette, visite en vol libre, remise à zéro, vitesse de marche, retour à la scène |
| `files` | Export `.ply` et `.splat`, réimport par sélecteur et par glisser-déposer, retour au direct, fichiers abîmés |
| `vr` | Session WebXR émulée (Quest 3): entrée, rendu des deux yeux, joysticks, maquette, sortie; panneau sans casque |
| `narrow` | Fenêtre de 400 px: barre du haut, panneau en feuille basse, commandes tactiles |
| `subpath` | Hébergement statique sous un sous-dossier, `?source=`, navigateur sans WebGL2 |
| `server` | Avec `gs-server`: WebSocket, configuration partagée entre deux clients, partage sur le réseau local, origine Tauri simulée, reconnexion. Ignorée si le binaire n'existe pas (`cargo build -p gs-server --release`) |
| `perf` | Tailles du build, cadence du worker et de l'affichage, durée d'un scan complet |

Variables d'environnement:

| Variable | Rôle |
|---|---|
| `SKIP_BUILD=1` | Utilise le `dist` existant |
| `ONLY=stage,vr` | Ne lance que ces suites |
| `GS_APP_OUT=dossier` | Captures d'écran et `results.json` (par défaut `out/app-e2e`) |
| `GS_APP_PORT=8110` | Premier port essayé par le serveur statique |
| `GS_SERVER_BIN=chemin` | Binaire `gs-server` de la suite `server` (par défaut `target/release/gs-server`) |
| `GS_NPM_TOOLS`, `IWER_BUNDLE` | Où trouver l'émulateur IWER |

Chaque suite se lance aussi seule: `node e2e/app/house.cjs` (`BUILD=1` pour
reconstruire avant).

Les scripts lisent l'état de l'application par `window.__splatLive.state()`
(un objet JSON: source, connexion, scène, configuration, calques,
statistiques, session VR, vignettes, scan, erreurs) et
`window.__splatLive.viewerEye()`.
