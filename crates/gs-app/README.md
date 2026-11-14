# gs-app : l'interface de Splat Live

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

Une troisième source s'ajoute à ces deux-là : un enregistrement (`.gsrec`),
relu sans pipeline (voir « Enregistrement et relecture 4D »).

La source active est affichée dans la barre du haut : « Serveur natif »,
« Local (WASM) » ou « Enregistrement », avec l'état de la connexion. La liaison WebSocket se
rétablit toute seule, avec un délai croissant jusqu'à 5 secondes.

Avec un serveur natif, le WebSocket n'est pas ouvert par la page mais par le
worker `splat-worker`, en mode relais. Une page occupée à dessiner lit son
propre socket trop lentement : les trames s'accumulent dans le navigateur et
dans les tampons TCP, et l'image prend des secondes de retard sans jamais les
rattraper. Le worker, lui, lit toujours le socket à temps ; il ne garde que le
dernier message de chaque flux « le plus récent gagne » (calque 1,
statistiques, vignette de chaque caméra), conserve dans l'ordre les messages
fiables (état, calque 0) et les remet à la page par lots acquittés, deux au
plus en attente. Une page lente affiche donc moins d'images, jamais des
images anciennes. Si le worker ne peut pas démarrer, ou ne parvient pas à
ouvrir de connexion, la page ouvre le WebSocket elle-même.

Un `cargo build` à la racine du dépôt compile ce paquet en simple coquille
(l'interface n'existe que pour `wasm32-unknown-unknown`) ; `cargo test -p gs-app`
exécute les tests de `src/logic`, qui n'ont pas besoin de navigateur.

## Paramètres d'adresse

| Paramètre | Effet |
|---|---|
| `?source=worker` | Force le worker local, sans chercher de serveur |
| `?source=server` | Force le serveur natif ; s'il ne répond pas, l'interface le dit et réessaie |
| `?scene=house` | Avec le worker : démarre directement sur le scan de maison |
| `?file=<adresse>` | Ouvre un enregistrement `.gsrec` (relu), ou une scène `.ply` / `.splat` (affichée comme un fichier importé). Chemin relatif ou adresse `http(s)` ; un autre site doit autoriser la lecture (CORS). Rien d'autre n'est accepté : ni `javascript:`, ni `data:`, ni `blob:`, ni `file:`, ni identifiants dans l'adresse |
| `?embed=1` | Visionneuse intégrée : le canevas et de petites commandes (lecture, position, vitesse, boucle, plein écran, VR), sans barre ni panneau. Pour une `<iframe>` |
| `?autoplay=1`, `?loop=1` | Lance la lecture, la boucle |
| `?t=<secondes>` | Position de départ |
| `?view=<ex,ey,ez,cx,cy,cz>` | Position de l'œil puis point visé, en mètres |
| `?nav=orbit` ou `fly` | Mode de navigation |

Une valeur refusée est écartée et signalée ; les autres paramètres
s'appliquent. Sans paramètre `file`, la page lit `share.json` à côté
d'`index.html` s'il existe (l'archive de partage du serveur en contient
un) : `{"file": "recording.gsrec", "autoplay": true, "loop": true}`, et les
clés `embed`, `t`, `view` (six nombres), `nav`. Les paramètres de l'adresse
l'emportent sur ce fichier, un par un. Le fichier nommé passe la même
validation qu'un lien.

Page d'exemple d'intégration : `e2e/app/embed.html`. Le code à coller est
celui que donne la boîte de partage :

```html
<iframe src="https://…/?file=recordings/danse.gsrec&embed=1&autoplay=1&loop=1"
        width="960" height="540" style="border:0"
        allow="fullscreen; xr-spatial-tracking" allowfullscreen></iframe>
```

## Enregistrement et relecture 4D

Le bouton d'enregistrement (barre du haut, touche R, section
« Enregistrement ») capte le flux que reçoit la visionneuse :

| Situation | Où va l'enregistrement |
|---|---|
| Serveur natif, page ouverte sur l'ordinateur du serveur | Sur le disque du serveur (`POST /api/record`). La liste vient de `GET /api/recordings` ; relecture par requêtes `Range`, téléchargement, suppression, partage |
| Page sans serveur, ou client du réseau local | Dans la mémoire du worker, jusqu'à 256 Mio, puis remis à la page comme `Blob` : relecture immédiate, téléchargement |

« Enregistrer le scan terminé » garde un scan (ou une capture) fini comme
enregistrement d'une seule image clé.

La relecture tourne dans un second worker `splat-worker`, en mode relecture
(`gs_worker::replay`, autour de `gs_core::record::Player`). Pendant ce
temps la source en direct attend : le pipeline local est suspendu, la
connexion au serveur est fermée puis rouverte. Un fichier de 64 Mio au plus
est tenu entier en mémoire (les déplacements sont alors immédiats) ; au-delà
le lecteur lit par morceaux (`Blob.slice`, ou requêtes `Range` si le serveur
les honore, sinon téléchargement continu). Dans le casque, la ligne de
temps est dessinée au-dessus de la main gauche : Y lance et suspend la
lecture, la gâchette latérale gauche tenue, le joystick gauche déplace la
tête de lecture.

La boîte « Partager » d'un enregistrement du serveur donne le lien sur le
réseau local, son QR code (dessiné par la page), le code d'intégration
`<iframe>` et l'archive web (`GET /api/share/<nom>.zip`). Pendant une
relecture ouverte par adresse, le bouton « lien » de la ligne de temps
copie un lien vers ce moment et ce point de vue (`t` et `view`).

## Capture avec un iPhone

Section « iPhone » du panneau. Avec le serveur natif, sur son ordinateur :
« Afficher le code d'appairage » active le partage sur le réseau local si
besoin et montre le QR code de `GET /api/pair.svg`. La capture se suit
ensuite comme un scan : mode, nom, appareil, images reçues et utilisées,
pause, fin, vignette et repère du téléphone ; quand la reconstruction
atteint sa capacité, le panneau le dit. Un fichier `.gscap` s'importe par
le même panneau : le serveur le reçoit (`POST /api/captures`) ; sans
serveur, le worker le reconstruit lui-même, image par image
(`gs_worker::capture`), ce qui permet d'ouvrir une capture sur un simple
hébergement statique. Le worker lit d'abord l'en-tête du fichier : un
fichier qui n'est pas une capture est refusé sans rien interrompre de ce
qui est à l'écran. Les images d'un vrai téléphone pèsent quelques centaines
de kilo-octets chacune, bien plus que le morceau lu d'un coup : le worker
lit la suite tant qu'une image est incomplète (`LocalCapture::wants_bytes`).

Ce qu'un enregistrement ou un flux fait construire à l'interface est borné
(`logic::present::bound_state`) : 64 caméras, 4096 points de trajet, noms
de 200 caractères, vignettes d'un mégapixel au plus. La corbeille d'un
enregistrement demande confirmation : un premier clic l'arme pour quatre
secondes, le second supprime.

Avant d'envoyer un fichier `.gscap` au serveur, la page en lit les huit
premiers octets : un fichier qui n'est pas une capture est refusé sur
place, avec un message clair, au lieu d'être envoyé en entier. Une capture
s'affiche vue de l'extérieur, une pièce vue de haut
(`logic::present::capture_view`), jamais depuis la dernière position du
téléphone ; le bouton pause disparaît pendant une capture (le serveur
l'ignore). Quand le disque du serveur est presque plein, ou qu'il a refusé
une capture, la page le dit (`storage` de `GET /api/record`).

## Captures conservées et affinage hors ligne

Section « Captures et affinage » du panneau (`ui/refine.rs`,
`core/refine.rs`, `logic/refine.rs`). Avec le serveur natif, sur son
ordinateur :

- la liste des captures que le serveur conserve (`GET /api/captures`) ;
  chacune se rouvre (`POST /api/captures/open`), se supprime, ou s'affine ;
- « Affiner cette capture » (`POST /api/refine`) : préréglage Rapide,
  Standard ou Fin avec sa durée mesurée, choix « peindre d'après les
  photos » ou « laisser les trous » là où le capteur n'a rien mesuré, et les
  exports COLMAP et nerfstudio, écrits dans un dossier de l'ordinateur que
  la page affiche ;
- le calcul en cours, interrogé chaque seconde (`GET /api/refine`) : étape,
  itération, temps écoulé et restant, « Annuler » (qui ne garde rien) ;
- les captures affinées (`GET /api/refined`) : « Voir le résultat » charge
  le `.ply` et l'affiche comme un fichier importé, en mélange trié (une
  pièce depuis l'endroit où le téléphone a commencé, en vol libre) ;
  téléchargement, partage (lien, `<iframe>`, archive) et suppression comme
  pour un enregistrement.

Un client du réseau local voit et ouvre les captures affinées, rien
d'autre. Sans serveur, la section tient en une phrase qui dit pourquoi.
Les textes disent ce que l'affinage apporte et ce qu'il n'apporte pas
(mesuré sur des captures simulées, processeur seulement) ; le score sur les
photos mises de côté n'est jamais affiché comme une qualité.

Dans la fenêtre de l'application de bureau, tout téléchargement passe par
la commande Tauri `export_recording`, puis `reveal` (une vue web n'a pas de
gestionnaire de téléchargements).

## Rendu

« Rendu » (section Affichage) : Auto, Doux, Net, Trié. Auto dessine les
surfels du pipeline en mélange normalisé, contours doux sur un écran et
nets dans un casque (personne n'a encore mesuré de vrai casque), et les
fichiers importés en mélange trié. Le panneau nomme le mélange réellement
actif, relu deux fois par seconde, et affiche un avertissement quand la
visionneuse a dû se replier sur le tri (carte graphique sans textures
flottantes, mémoire graphique insuffisante). La taille des splats suit la
scène et le mode (scène 1 ; maison 0,8 en contours doux, 1 en contours
nets et en tri) tant que le curseur n'a pas été touché. Le choix du rendu
est mémorisé dans le navigateur (`localStorage`, clé `splat-live.blend`) ;
c'est le seul réglage local conservé, les autres appartiennent au pipeline.

## Raccourcis

| Touche | Effet |
|---|---|
| H | Masque ou réaffiche l'interface |
| P | Pause, reprise (du pipeline, ou de la lecture d'un enregistrement) |
| R | Démarre ou arrête l'enregistrement |
| K | Lecture, pause d'un enregistrement |
| J, L | Recule, avance de 5 secondes |
| , et . | Recule, avance d'un vingtième de seconde |
| Début, Fin | Va au début, à la fin |
| B | Lecture en boucle |
| Échap | Réaffiche l'interface |
| ZQSD ou WASD, flèches | Vol libre (touches physiques) |
| E ou Espace, A (Q) | Monter, descendre |
| Maj | Aller plus vite |

Les touches de vol sont ignorées pendant la saisie dans un champ.

## Organisation du code

| Module | Rôle |
|---|---|
| `src/logic` | Tout ce qui se calcule sans navigateur : choix de la source, correctifs de configuration, présentation de la scène (repères, vue maquette, cadrage), couleurs de profondeur, formats de fichiers, mise en forme des nombres, paramètres d'adresse et liens de partage (`share`), relecture et enregistrements (`replay`), capture (`capture`), choix du rendu (`render`), QR codes (`qr`) |
| `src/core.rs` | Le viseur, le transport et le traitement des messages ; `src/core/replay.rs` (relecture), `record.rs` (enregistrement, liste, partage), `capture.rs` (appairage, import de capture), `refine.rs` (captures conservées, affinage, résultats) |
| `src/transport` | `Transport::Server` (WebSocket, relayé par le worker ou ouvert par la page) et `Transport::Worker` (pipeline dans le Web Worker) : mêmes octets, mêmes commandes ; `ReplayLink`, le worker de relecture |
| `src/state.rs` | Les signaux réactifs lus par les vues |
| `src/ui` | Barre du haut, panneau latéral, éléments superposés |
| `src/debug.rs` | `window.__splatLive` pour les vérifications automatiques |
| `worker-boot.js` | Chargeur du worker, versionné par l'identifiant de build |

La configuration affichée est toujours celle que le pipeline renvoie dans son
message `State` : un réglage modifié par un autre client apparaît donc aussi.
Les modifications partent en `patch_config`, regroupées après un court délai.

## Point d'accès pour les tests

`window.__splatLive.state()` renvoie un objet JSON (entre autres `replay`
avec l'état du lecteur et la latence des déplacements, `recording`,
`takes`, `capture`, `import`, `pair`, `embed`, `viewer.blend`, `captures`,
`refined`, `job`, `jobEnd`, `storage`) : `source`, `channel`
(`worker`, `relay` ou `direct`), `link`, `connected`, `scene`, `config`,
`layers`, `viewer`, `nav`, `eye`, `target`, `display`, `xr`, `pipelineHz`
(cadence mesurée sur le compteur de ticks), `stats`, `counts`,
`previewCount`, `previews`, `scan`, `file`,
`fileMode`, `fileError`, `notice`, `transferBytesPerSecond`, `server`,
`panelOpen`, `uiHidden`, `errors`. `window.__splatLive.viewerEye()` renvoie la
position de la caméra `[x, y, z]`.

Les vérifications de bout en bout sont dans `e2e/app` (`node e2e/app/run.cjs`).
