# Vérifications de bout en bout de l'application

Playwright pilote la vraie application (`crates/gs-app/dist`) dans Chromium
sans affichage. Le rendu WebGL2 y est logiciel (SwiftShader) : les images sont
justes, les cadences mesurées sont un plancher.

```
node e2e/app/run.cjs
```

Cette commande construit l'application (`trunk build --release` dans
`crates/gs-app`), la sert avec un petit serveur statique sur un port de la
plage 8100 à 8199, puis lance toutes les suites. Playwright et
l'émulateur WebXR IWER s'installent une fois depuis le dossier `e2e` :
`(cd e2e && npm install && npx playwright install chromium)`. Sur une machine
où Playwright est installé ailleurs, `NODE_PATH` doit y mener ; l'émulateur
seul peut aussi venir de `GS_NPM_TOOLS`.

| Suite | Ce qu'elle vérifie |
|---|---|
| `stage` | Mode worker : source détectée, image non vide et animée, statistiques, vignettes RGB et profondeur, pause, nombre de caméras, résolution, bruit, ombres, calques, repères, orbite, contrôle de flux du worker, raccourcis H et P |
| `house` | Scan de maison : construction progressive, pause, « Terminer maintenant », vue maquette, visite en vol libre, remise à zéro, vitesse de marche, retour à la scène |
| `files` | Export `.ply` et `.splat`, réimport par sélecteur et par glisser-déposer, retour au direct, fichiers abîmés |
| `render` | Avec les réglages de rendu de l'application (les autres suites sont en mélange trié, voir plus bas) : mélange normalisé par défaut, niveaux Doux, Net, Trié, taille des splats par scène et par mode, repli annoncé sur une carte sans textures flottantes, choix mémorisé, contours nets par défaut dans le casque, tailles de cellule du scan, préréglage léger du worker |
| `replay` | Relecture d'un enregistrement par lien, par fichier, par glisser-déposer et depuis la mémoire ; ligne de temps, raccourcis clavier, vitesse, boucle, lien vers un moment ; paramètres d'adresse et valeurs refusées (`javascript:`, `data:`, `blob:`, `file:`, identifiants) ; hôte sans `Range`, autre origine avec et sans CORS, enregistrement d'une minute lu par requêtes `Range`, archive de partage (`share.json`) ; latence des déplacements ; enregistrement en mémoire, téléchargement vérifié par `tools/gsrec.py`, scan terminé enregistré ; relecture dans le casque émulé (manette gauche) |
| `embed` | Mode `embed=1` seul, puis dans une `<iframe>` d'une page d'une autre origine (`embed.html`) : commandes, plein écran, VR depuis le cadre, cadre étroit |
| `capture` | Fichier `.gscap` ouvert sans serveur : le worker le reconstruit image par image, la page suit la capture ; arrêt, fichier tronqué, fichier invalide, capture terminée enregistrée et relue |
| `vr` | Session WebXR émulée (Quest 3) : entrée, rendu des deux yeux, joysticks, maquette, sortie ; panneau sans casque |
| `narrow` | Fenêtre de 400 px : barre du haut, panneau en feuille basse, commandes tactiles |
| `layout` | Mise en page à 400, 800 et 1600 px (et quelques tailles ingrates) : rien ne dépasse, aucun texte coupé, aucun recouvrement entre barre, panneau, pastilles et messages ; barre du haut à toutes les largeurs de 761 à 1300 px |
| `subpath` | Hébergement statique sous un sous-dossier, `?source=`, navigateur sans WebGL2 |
| `server` | Avec `gs-server` : WebSocket, configuration partagée entre deux clients, partage sur le réseau local, origine Tauri simulée, reconnexion. Ignorée si le binaire n'existe pas (`cargo build -p gs-server --release`) |
| `studio` | Avec `gs-server` : enregistrement piloté par le bouton, liste, relecture, téléchargement, suppression ; boîte de partage (lien du réseau local, QR code, `<iframe>`, archive décompressée et servie telle quelle) ; lien ouvert depuis un autre appareil ; client du réseau local qui enregistre en mémoire ; appairage d'un iPhone et capture suivie pendant que `tools/fake_phone.py` en joue une ; fichier `.gscap` envoyé au serveur ; scan terminé enregistré |
| `refine` | Avec `gs-server` (`--refine-iterations 150`) : « Affiner cette capture » de bout en bout sur une chambre filmée par le téléphone simulé de gs-core. Un fichier qui n'est pas une capture est refusé sans envoi ; la capture envoyée est conservée et listée ; une pièce terminée s'ouvre vue de haut, sans bouton pause ; le formulaire dit ce que l'affinage apporte et coûte ; un calcul est suivi puis annulé (rien n'est gardé), un autre mené à son terme ; le résultat s'affiche (fichier importé, mélange trié, de l'intérieur), se télécharge, se partage (lien du réseau local ouvert dans un autre contexte, archive), se supprime ; export COLMAP ; capture rouverte depuis la liste ; client du réseau local ; sans serveur, une phrase dit pourquoi la fonction est absente. Images `refine-*.png` |
| `shared` | Avec `gs-server`, plusieurs clients : une page lente ou figée ne prend pas de retard sur le pipeline (relais du WebSocket par le worker), les commandes suivent l'état renvoyé par le pipeline quel que soit le client qui les change, arrivée en cours de scan, remplacement du serveur pendant un scan, repli sur un WebSocket ouvert par la page quand le worker relais ne démarre pas |
| `lanvr` | Avec `gs-server --lan` : l'accès https du réseau local avec un Quest 3 émulé, entrée en VR, deux yeux, déplacement, maquette, sortie, maison en VR ; ordinateur doté de son propre casque |
| `webkit` | Le même build dans WebKitGTK (MiniBrowser sous Xvfb), le moteur de l'application de bureau sous Linux : mode serveur par le relais, mode worker sur la scène et dans la maison. Ignorée si MiniBrowser, `xvfb-run` ou `import` manquent |
| `soak` | Mémoire sur la durée (`SOAK_S`, 40 s par défaut, 180 pour une vraie vérification) : tas JavaScript, mémoire WebAssembly de la page et du worker, nœuds du document, en mode worker (scène, maison scannée en boucle) et en mode serveur |
| `perf` | Tailles du build, cadence du worker et de l'affichage, durée d'un scan complet |
| `shots` | Sur demande seulement (`ONLY=shots`) : images du produit à densité double, avec le rendu par défaut, dans `<sortie>/shots` (scène et maison terminée sur un écran et dans le casque émulé, ligne de temps, boîte de partage, visionneuse intégrée dans une page, appairage, capture en cours) |

Les suites fixent le rendu sur « Trié » (réglage mémorisé de
l'application, posé avant le chargement de la page) : sur le rasteriseur
logiciel d'un navigateur sans affichage, le mélange normalisé coûte 2 à
4,5 fois plus par image, et la plupart des vérifications attendent sur
l'horloge. La suite `render` et les images de `shots` utilisent le rendu
par défaut de l'application.

Variables d'environnement:

| Variable | Rôle |
|---|---|
| `SKIP_BUILD=1` | Utilise le `dist` existant |
| `ONLY=stage,vr` | Ne lance que ces suites |
| `GS_APP_OUT=dossier` | Captures d'écran et `results.json` (par défaut `out/app-e2e`) |
| `GS_APP_PORT=8110` | Premier port essayé par le serveur statique |
| `GS_SERVER_BIN=chemin` | Binaire `gs-server` des suites `server`, `studio`, `refine`, `shared`, `lanvr` et `soak` (par défaut `target/release/gs-server`) |
| `SOAK_S=180` | Durée de la suite `soak`, en secondes |
| `LONG_REPLAY=0` | Saute l'enregistrement d'une minute de la suite `replay` (120 Mo sur le disque) |
| `GS_APP_SHOTS=dossier` | Où la suite `shots` écrit ses images |
| `WEBKIT_S=25`, `WEBKIT_MINIBROWSER=chemin` | Durée de chaque session de la suite `webkit` et emplacement de MiniBrowser |
| `GS_NPM_TOOLS`, `IWER_BUNDLE` | Où trouver l'émulateur IWER |

Chaque suite se lance aussi seule : `node e2e/app/house.cjs` (`BUILD=1` pour
reconstruire avant).

Les scripts lisent l'état de l'application par `window.__splatLive.state()`
(un objet JSON : source, canal de transport, connexion, scène, configuration,
calques, statistiques, cadence mesurée, session VR, vignettes, scan, erreurs) et
`window.__splatLive.viewerEye()`.
