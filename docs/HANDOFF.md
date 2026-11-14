# Passation : Splat Live sur le Mac

Ce document s'adresse à l'agent (ou à la personne) qui reprend le dépôt sur
le MacBook Pro M5 Max du propriétaire. Il dit ce qui existe, ce qui a été
vérifié et comment, ce qui n'a jamais tourné, et dans quel ordre s'y prendre.

Tout le développement s'est fait dans un conteneur Linux à 4 cœurs, sans
carte graphique, sans macOS, sans Xcode, sans iPhone et sans casque. Cette
phrase explique presque tous les risques listés plus bas.

Documents à lire avant de toucher au code :

- `docs/ARCHITECTURE.md` : la spécification, contraignante. Conventions
  (mètres, repère main droite, +Y vers le haut), formats binaires, routes,
  comportements. Si le code et ce document divergent, c'est un défaut : on
  corrige l'un ou l'autre, on ne laisse pas l'écart.
- `README.md` : usage, pour le propriétaire.
- `ios/SplatCapture/README.md` : l'application iPhone.
- `docs/RECHERCHE_QUALITE.md` : ce qui a été essayé pour la qualité du rendu
  temps réel, avec les mesures.

## 1. Ce que fait le produit

Des capteurs couleur et profondeur (simulés, ou un iPhone Pro avec LiDAR)
alimentent une reconstruction en temps réel : chaque pixel de profondeur
devient une gaussienne plate, fusionnée dans une grille de voxels. Une
visionneuse WebGL2 affiche le résultat, aussi en réalité virtuelle (WebXR).

| Partie | Où | Rôle |
|---|---|---|
| Cœur | `crates/gs-core` | scènes simulées, capteurs, reconstruction, fusion, formats, lecture des captures iPhone (`ingest`), enregistrement 4D (`record`) |
| Rendu | `crates/gs-render` | visionneuse WebGL2 et WebXR, deux modes de mélange |
| Serveur | `crates/gs-server` | HTTP local, HTTPS sur le réseau local, flux vers les visionneuses, réception de l'iPhone, enregistrements, partage, affinage |
| Interface | `crates/gs-app`, `crates/gs-worker` | interface Leptos, travail en arrière-plan dans un worker |
| Bureau | `src-tauri` | coquille Tauri 2 qui embarque le serveur et l'interface |
| Affinage hors ligne | `crates/gs-refine` | entraînement 3DGS sur CPU à partir d'une capture, export COLMAP et nerfstudio |
| iPhone | `ios/SplatCapture` | application compagnon (ARKit, LiDAR) |
| Outils | `tools/` | implémentation de référence des formats en Python, faux téléphone, faux serveur |
| Tests de bout en bout | `e2e/render`, `e2e/app`, `e2e/server` | Playwright et scripts Python contre les vrais binaires |

## 2. État exact

### Vérifié ici, par des tests relancés sur l'arbre final

Tout a tourné sous Linux, dans une machine virtuelle à 4 cœurs sans carte
graphique.

| Suite | Résultat sur l'arbre livré |
| --- | --- |
| `cargo test --no-fail-fast` (36 binaires de test) | 783 réussis, 0 échec, 7 ignorés. Relancé deux fois, par le relecteur final puis par l'orchestrateur. |
| `cargo clippy --workspace --all-targets` | 0 avertissement |
| `cargo clippy --target wasm32-unknown-unknown -p gs-app -p gs-worker -p gs-render` | 0 avertissement |
| `cargo test -p splat-live` (coquille Tauri) | 4 réussis |
| `python3 -m unittest discover -s tools` | 88 réussis |
| `e2e/render` | 379 sur 379 |
| `e2e/server` | `capture.py` 79, `refine.py` 41, `hostile.py` 102, `abuse.py` 40, `browser.cjs` 18, `real-frontend.cjs` 27, `share.cjs` 40, `tauri-smoke.sh` 21, `tauri-real.sh --debug` 15 : tous réussis |
| `e2e/app` | 499 sur 500 en passe complète. L'échec est un contrôle sensible au temps, décrit en section 6. |

Deux précisions sur l'ordre des passes : `e2e/render` et la passe complète de
`e2e/app` ont tourné avant le tout dernier changement du serveur (le
classement des adresses réseau, une fonction dans `net.rs`). Après ce
changement ont été relancés tout `e2e/server` et les suites `capture`,
`server`, `studio`, `refine`, `shared` et `lanvr` de `e2e/app` (166 sur 167,
l'échec étant l'autre contrôle sensible au temps de la section 6).

Vérifications faites une fois à la main par le relecteur final, dont les
scripts ne sont pas dans le dépôt :

- 110 scénarios sur le travail d'affinage du serveur : noms piégés, règles
  d'origine et de réseau, huit démarrages simultanés, annulation puis
  relance, capture supprimée en cours de travail, captures abîmées ou
  vides, arrêt du serveur par SIGTERM et SIGKILL en plein travail.
- Disque réellement plein (volume en mémoire de 300 Mo) : 18 contrôles.
- Le parcours complet par l'interface : téléphone simulé qui envoie 220
  images, fin de capture, affinage `rapide` réel pendant que la capture est
  encore affichée (118 secondes), puis « Voir le résultat ».

Chaque lot a été écrit par un agent puis attaqué par un second agent
indépendant, chargé de le réfuter. Ces vérificateurs ont trouvé et corrigé de
vrais défauts à chaque fois ; leurs listes de points restants sont reprises
en section 6.

### Écrit, jamais compilé ni exécuté

- **Toute l'application iPhone** (`ios/SplatCapture`, Swift). Aucune
  chaîne d'outils Swift n'était disponible. Le code a été relu contre la
  documentation des API, et ses formats sont vérifiés indirectement : les
  mêmes fichiers d'exemple (`ios/SplatCapture/Fixtures`) sont lus par les
  tests Swift (jamais lancés), Python et Rust (lancés). Il faut s'attendre à
  des erreurs de compilation.
- **Le serveur et la coquille Tauri sous macOS.** Compilés et testés sous
  Linux uniquement (WebKitGTK, Xvfb).

### Jamais confronté au réel

- **Aucune image d'un vrai iPhone.** Le téléphone simulé a un bruit de
  profondeur blanc par pixel, un point principal exactement centré et des
  images sans distorsion. Le LiDAR réel a un bruit lisse dans l'espace et
  variable d'une image à l'autre, et ARKit donne un point principal décalé
  de quelques pixels.
- **Aucune carte graphique.** Le rendu a tourné dans Chromium sans écran,
  sur SwiftShader (rendu logiciel). Les performances mesurées ne valent que
  comme ordre relatif. Le mode de mélange normalisé y est 3 à 5 fois plus
  lent que le mode trié ; sur un vrai GPU, on ne sait pas.
- **Aucun casque.** La réalité virtuelle a été testée avec l'émulateur IWER
  uniquement.
- **Aucun autre navigateur que Chromium.** Ni Safari, ni Firefox, ni
  WKWebView (la vue web de Tauri sous macOS).

## 3. Premières tâches, dans l'ordre

Chaque étape a un critère de réussite. Ne pas passer à la suivante avant.

### Étape 1 : compiler et tester le cœur

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk
cargo build
cargo test --no-fail-fast
cargo test -p gs-core --release --features par
python3 -m unittest discover -s tools
```

Réussite : les mêmes comptes que dans la section 2. Les tests de cadence
(noms contenant `hz`, `timing`, `steadily`) sont sensibles à la charge de la
machine : les relancer seuls avant de conclure à un défaut.

Ne jamais exporter `RUSTFLAGS` : `.cargo/config.toml` fixe déjà
`--cfg=web_sys_unstable_apis` pour wasm32, et la variable l'écraserait.

### Étape 2 : l'interface web et le serveur

```bash
(cd crates/gs-app && trunk build --release)
cargo run --release -p gs-server -- --lan --dist crates/gs-app/dist
```

Ouvrir `http://127.0.0.1:7878` dans Chrome d'abord (le navigateur testé),
puis dans Safari.

Réussite : la scène en direct (une danseuse entourée de 8 caméras) s'anime ;
le scan de maison se déroule ; le panneau latéral indique le mode de mélange
actif. Regarder en premier la fréquence d'images affichée en bas à gauche,
en mode normalisé puis trié : c'est la première mesure sur un vrai GPU.

À surveiller : le mode normalisé a besoin de cibles de rendu en demi-flottant
(`EXT_color_buffer_half_float` ou `EXT_color_buffer_float`). Si l'extension
manque, la visionneuse doit retomber sur le mode trié et le dire
(`Viewer::blend_status()`). Vérifier que c'est bien ce qui se passe dans
Safari, et non un écran noir.

### Étape 3 : l'application de bureau

```bash
cargo install tauri-cli
cargo tauri dev        # depuis la racine ou src-tauri
cargo tauri build
```

Voir `src-tauri/README.md`. Réussite : la fenêtre s'ouvre sur la même
interface ; l'enregistrement, l'export d'un enregistrement et l'ouverture du
dossier dans le Finder fonctionnent (la commande `reveal` n'a été exercée sur
aucune plateforme).

WKWebView n'a pas WebXR : le bouton VR doit simplement être absent ou
inactif, sans erreur.

### Étape 4 : l'application iPhone

```bash
brew install xcodegen
cd ios/SplatCapture
(cd Packages/SplatCaptureKit && swift test)
xcodegen generate
open SplatCapture.xcodeproj
```

1. Corriger d'abord le paquet `SplatCaptureKit` jusqu'à ce que `swift test`
   passe. Il ne dépend ni d'ARKit ni d'UIKit et contient tout ce qui touche
   aux formats : c'est la partie la plus sûre, ses tests comparent octet par
   octet avec les fichiers d'exemple.
2. Dans Xcode, choisir l'équipe de signature (ou `DEVELOPMENT_TEAM` dans
   `project.yml` ; identifiant `be.splatlive.capture`), activer le mode
   développeur sur l'iPhone, compiler.
3. Corriger les erreurs de compilation de l'application. Les endroits les
   plus incertains sont listés en section 5.

Règle absolue pendant ces corrections : **ne jamais modifier un format pour
faire passer le code Swift.** Les formats sont fixés par
`docs/ARCHITECTURE.md`, par `tools/gscap.py` et par les fichiers d'exemple ;
le serveur Rust les lit déjà. Si le Swift et les fichiers d'exemple
divergent, c'est le Swift qui a tort.

### Étape 5 : première capture, sans le serveur Rust

```bash
python3 tools/fake_server.py --save out/captures
```

Coller dans l'application l'adresse d'appairage affichée, filmer quelques
secondes, terminer, puis :

```bash
python3 tools/gscap.py verify out/captures/<fichier>.gscap
python3 tools/gscap.py info out/captures/<fichier>.gscap
```

Réussite : `verify` ne signale rien. Ce fichier est la première donnée
réelle du projet : **le conserver précieusement**, il sert à tout le reste.
Vérifier dans `info` que les dimensions, les intrinsèques (point principal
proche du centre de l'image couleur) et l'état de suivi sont plausibles.

### Étape 6 : première capture avec le vrai serveur

Lancer le serveur avec `--lan` (étape 2) ou l'application de bureau, ouvrir
la section « iPhone » du panneau, scanner le QR code depuis l'application,
choisir le mode « pièce », filmer.

Réussite : la reconstruction apparaît et grandit dans la visionneuse pendant
que l'on filme ; à la fin, la capture figure dans les enregistrements.

Si l'image est fausse, vérifier dans cet ordre :

1. **La pose.** ARKit donne `camera.transform` (caméra vers monde, axes de la
   caméra : +X à droite, +Y en haut, -Z vers l'avant, en orientation paysage
   native). `gs_core::ingest` refuse une pose transposée ou en miroir, mais
   une rotation d'un quart de tour passerait. Le test
   `crates/gs-core/tests/ingest_verify.rs` (scène de mur calculée à la main)
   dit exactement ce qui est attendu.
2. **Les intrinsèques.** Elles se rapportent à l'image couleur et sont mises
   à l'échelle de la carte de profondeur par le serveur, sans correction du
   demi-pixel.
3. **L'orientation.** La géométrie n'est jamais tournée ; seul l'aperçu
   l'est, selon l'octet d'orientation.
4. **La profondeur.** En millimètres, `floor(m * 1000 + 0.5)`, 0 pour
   « pas de mesure ».

Rejouer ensuite le fichier `.gscap` de l'étape 5 autant de fois que
nécessaire, sans le téléphone :

```bash
cargo run --release -p gs-core --features par --example gscap_ingest -- <fichier>.gscap
```

Les options `--voxel`, `--far`, `--min-confidence`, `--max-voxels` et
`--layer-window` permettent d'essayer d'autres réglages sur la même capture.

### Étape 7 : le casque

Sur le réseau local, ouvrir dans le navigateur du casque l'adresse `https`
affichée dans la section « Casque VR » (ou scanner son QR code), comparer
l'empreinte du certificat, l'accepter une fois, entrer en VR.

Réussite : la scène est stable, à l'échelle, sans décalage entre les yeux.
Le niveau de qualité « dur » est proposé par défaut en VR ; mesurer la
fréquence d'images dans les deux niveaux.

## 4. Affinage hors ligne

### Ce qui existe

`crates/gs-refine` prend une capture `.gscap`, part du nuage que la fusion
temps réel en tire, et optimise des gaussiennes 3D contre les photographies
(L1, D-SSIM, terme de profondeur LiDAR, densification, correction
d'exposition par image). Le résultat est un `.ply` au format INRIA que la
visionneuse ouvre. Tout tourne sur le CPU, avec rayon.

```bash
cargo run --release -p gs-refine -- <capture>.gscap -o <sortie>.ply --preset rapide
cargo run --release -p gs-refine -- info <capture>.gscap
cargo run --release -p gs-refine -- export <capture>.gscap --format colmap -o <dossier>
cargo run --release -p gs-refine -- export <capture>.gscap --format nerfstudio -o <dossier>
cargo run --release -p gs-refine -- aide
```

Le même affinage se lance depuis l'interface, section « Captures et affinage »
du panneau : liste des captures conservées, choix du préréglage, choix
« peindre au-delà du capteur » ou « laisser les trous », progression,
annulation, puis « Voir le résultat », téléchargement et partage. Le serveur
exécute un seul travail à la fois, sur un fil dédié dans son propre processus
(`crates/gs-server/src/refine.rs`), avec tous les cœurs sauf un par défaut.

| Route | Rôle |
| --- | --- |
| `POST /api/refine` | `{"action":"start","capture":"x.gscap","preset":"rapide"}`, `{"action":"export","capture":"x.gscap","format":"colmap"}` ou `{"action":"cancel"}`. Boucle locale uniquement. |
| `GET /api/refine` | Travail en cours (étape, itération, temps restant estimé) et fin du dernier travail. |
| `GET /api/refined` | Liste des résultats. |
| `GET`, `HEAD`, `DELETE /refined/<nom>.ply` | Le fichier, avec `Range`. Suppression en boucle locale. |
| `GET /api/share/refined/<nom>.zip` | Archive autonome : visionneuse et scène. |

À savoir :

- Les résultats sont dans `<données>/refined/<capture>-<préréglage>.ply`, avec
  un `.json` voisin qui contient le rapport complet de gs-refine.
- Un export COLMAP ou nerfstudio écrit un dossier dans `<données>/datasets/`.
  L'interface affiche son chemin, elle ne le télécharge pas.
- Un travail annulé ne garde rien et n'écrit pas de point de reprise. Pour un
  travail très long ou à reprendre, utiliser la ligne de commande avec
  `--reprise`.
- Le score PSNR sur images retenues n'est jamais affiché : il ne mesure pas la
  qualité en vue libre (voir plus bas).
- Les durées annoncées dans l'interface (2 minutes en rapide, 10 minutes en
  standard) ont été mesurées sur 220 photos simulées et 3 fils d'une machine
  virtuelle. Une vraie capture en 1920x1440 de plusieurs minutes prendra plus
  longtemps. Le préréglage `fin` n'a jamais été exécuté de bout en bout.
- Options du serveur : `--refined-dir`, `--datasets-dir`, `--refine-threads N`,
  et `--refine-iterations N` qui sert aux tests (un résultat obtenu ainsi est
  flou, c'est attendu).
- Sur le M5 Max, essayer `--refine-threads` égal au nombre de cœurs
  performants et comparer le temps par itération lu dans `GET /api/refine`.

### Ce qui a été mesuré (captures simulées uniquement)

- Aux points de vue du téléphone et près de son trajet : plusieurs décibels
  de gain (chambre : 29,9 à 37,0 dB).
- Sur des vues éloignées du trajet, là où le nuage fusionné est déjà fermé :
  gain faible ou nul (préréglage standard : 29,40 à 29,43 dB). Le gain se
  concentre sur les contours et le voisinage des trous.
- Avec des défauts réalistes (bruit, dérive de pose de 1 cm, exposition) et
  une autre graine : +0,13 dB sur les vues éloignées, +2,7 dB aux points de
  vue du téléphone.
- Défauts visibles : une légère marbrure sur les surfaces planes, non
  expliquée ; un voile peint à la mauvaise profondeur là où le capteur n'a
  pas de mesure (fenêtres, miroirs, au-delà de 5 m).
- Vitesse sur un fil du conteneur, 140 000 gaussiennes : 77, 130, 205 et
  405 ms par itération à 320, 480, 640 et 960 pixels de large. Quatre fils
  vont 3,1 fois plus vite.
- Seul le préréglage rapide a été mené au bout par son auteur ; le standard
  l'a été une fois par le vérificateur ; le préréglage fin, jamais (mémoire
  seulement).

**Le score affiché par l'outil sur ses vues mises de côté n'est pas une
mesure de qualité en vue libre** : ces vues sont voisines des vues
d'entraînement. Il a affiché +9,5 dB là où les vues éloignées gagnaient 1,3.
Sur données réelles il n'y aura pas de vérité terrain : juger à l'œil, en se
déplaçant loin du trajet de capture.

### À essayer en premier sur une vraie capture

1. Le préréglage rapide, puis standard, sur la capture de l'étape 5.
2. `--mesure-seule` : laisse des trous propres au lieu du voile. C'est un
   choix pour le propriétaire, à faire sur une vraie pièce.
3. `--set train.loss.ssim=0` : +0,4 à 0,5 dB sur les vues éloignées en
   simulation, un tiers de gaussiennes en moins, un quart de temps en moins,
   au prix de 0,5 dB aux points de vue du téléphone.
4. `--poses` uniquement si les poses semblent bruitées (résultat dédoublé) :
   sur une capture propre, cette option dégrade.

### Raccourci à évaluer avant d'écrire une version GPU

L'export COLMAP ou nerfstudio permet d'entraîner la même capture avec un
outil existant. Deux candidats à vérifier sur le Mac (non vérifiés ici, le
réseau du conteneur ne le permettait pas) : **Brush** (Rust, wgpu, donc
Metal) et **OpenSplat** (C++, accélération Apple). Si l'un d'eux donne de
bons résultats sur une capture réelle, il fournit à la fois une référence de
qualité et peut-être la réponse la plus simple. Les exports ont été vérifiés
par des lecteurs écrits d'après la documentation des formats, jamais par
l'outil réel. Point d'attention : l'entraîneur 3DGS de référence ignore
`cx`, `cy` en lisant COLMAP, alors qu'ARKit donne un point principal
décentré.

### Plan pour une version GPU

Objectif : le même optimiseur, sur Metal à travers wgpu, validé contre la
version CPU qui sert d'oracle.

1. **Garder la version CPU intacte.** Elle est déterministe (identique au
   bit près pour 1 ou 3 fils, et à travers un point de reprise) et ses
   gradients analytiques sont vérifiés par différences finies contre une
   seconde implémentation en f64 (`crates/gs-refine/tests/gradients.rs`).
2. **Porter la passe avant** (projection, répartition en tuiles, tri par
   tuile, composition) en shaders de calcul. Critère : sur des scènes
   fixes, image identique à celle du CPU à plus de 50 dB, la même barre que
   celle déjà tenue entre `gs-refine` et `gs_core::raster`.
3. **Porter la passe arrière.** Critère : gradients égaux à ceux du CPU à
   1e-3 près en relatif sur les scènes des tests de gradient (recouvrement,
   découpe, plan proche, sous-pixel, saturation).
4. **Porter la perte** (L1, SSIM à fenêtre 11x11, terme de profondeur), ou la
   laisser sur le CPU dans un premier temps : elle ne coûtait que 16 % d'un
   pas avant d'être parallélisée.
5. **Garder sur le CPU** Adam épars, la densification et l'élagage tant que
   le profil ne dit pas le contraire.
6. **Valider de bout en bout** : même graine, même capture simulée, courbe
   de perte et scores finaux proches de ceux du CPU (le tri en flottant 32
   bits empêchera l'identité au bit près).
7. Seulement ensuite, régler pour le GPU : plus d'itérations, pleine
   résolution des photos (1920x1440), plus de gaussiennes.

Les notes de conception de l'auteur, y compris tout ce qui a été essayé puis
retiré avec la mesure correspondante, sont dans la documentation du crate
(en tête de `crates/gs-refine/src/lib.rs`).

## 5. Risques au premier contact

Classés par ordre probable de rencontre. Les numéros de ligne valent pour
l'arbre livré.

1. **Erreurs de compilation Swift.** Rien sous `ios/SplatCapture` n'a jamais
   été compilé. Commencer par
   `cd ios/SplatCapture/Packages/SplatCaptureKit && swift test` (Swift pur,
   fichiers d'exemple partagés), puis `xcodegen generate` et la compilation.
   Les usages d'API les plus denses : `App/CaptureEngine.swift`
   (configuration ARKit 152-179, classification des plans 357-380, délégué
   d'images 559-661), `App/FrameEncoder.swift` (JPEG par CoreImage 110-129,
   copies de `CVPixelBuffer` 134-212), `App/ServerLink.swift` (délégué
   URLSession, épinglage du certificat 97-123), `App/CaptureView.swift`
   271-360 (ARSCNView, lancer de rayon). `project.yml` fixe `SWIFT_VERSION`
   5.0 avec la concurrence stricte minimale.
2. **Le téléphone ne joint pas le Mac.** Vérifier dans cet ordre :
   - le pare-feu de macOS peut bloquer ou demander une autorisation pour les
     connexions entrantes sur `0.0.0.0:7879` d'un binaire non signé, et de
     nouveau après chaque recompilation ;
   - iOS demande la permission « réseau local » à la première connexion, et
     cette tentative échoue. Le flux réessaie (`ServerLink.swift:343-365`)
     mais pas `probe` (`ServerLink.swift:410`) : « joignable » peut afficher
     faux une fois ;
   - mauvaise adresse dans le QR code quand le Mac a plusieurs interfaces.
     Le classement est dans `crates/gs-server/src/net.rs:38` et l'interface
     n'utilise que l'adresse 0 (`crates/gs-app/src/core.rs:1172`). Essayer
     `curl 'http://127.0.0.1:7878/api/pair?i=1'` ;
   - le certificat est renouvelé quand l'adresse du Mac change
     (`crates/gs-server/src/tls.rs:197-221`) et le jeton change à chaque
     démarrage du serveur. Dans les deux cas il faut scanner de nouveau le
     QR code.
3. **Poignée de main WebSocket depuis `URLSessionWebSocketTask`.** L'écoute
   du réseau local annonce l'ALPN `h2` puis `http/1.1`
   (`crates/gs-server/src/tls.rs:80`). `/ingest` n'a été piloté que par des
   clients Python et Rust en HTTP/1.1, et les envois en HTTP/2 uniquement
   par curl. Si le téléphone ne reçoit pas `welcome` dans ses 8 secondes
   (`ServerLink.swift:51`), lancer le serveur avec `--verbose` et essayer
   une compilation où cette liste ALPN ne contient que `http/1.1`.
4. **De vraies images dans la chaîne.** Aucune image d'iPhone n'a jamais été
   ingérée : point principal décentré, vrai bruit et vraie confiance du
   LiDAR, dérive d'ARKit. L'application envoie par défaut la couleur en 960
   pixels de large (`CaptureEngine.swift:11`). Surveiller `frames_used`
   face à `frames_received` dans la carte de capture et le drapeau `used`
   des accusés de réception ; la fusion est dans
   `crates/gs-core/src/ingest/session.rs`. Garder le premier `.gscap`
   (section « Captures et affinage ») : c'est le premier vrai fichier de
   test.
5. **WKWebView (la fenêtre Tauri) et Safari n'ont jamais exécuté la
   visionneuse.** Regarder `#blend-status` dans le panneau
   (`crates/gs-app/src/ui/sections.rs:267`). S'il indique le mode trié, les
   cibles en demi-flottants ont été refusées
   (`crates/gs-render/src/normalized.rs:493`, message dans `splats.rs:181`)
   et une pièce terminée vue de dessus ressemble à une boîte fermée. Si la
   fenêtre est vide, suspecter le worker en module
   (`crates/gs-app/src/transport/worker.rs:26-28`) ou la CSP
   (`src-tauri/tauri.conf.json:32`) sous `tauri://localhost` ; lancer avec
   `SPLAT_LIVE_VERBOSE=1` et une compilation de débogage pour avoir
   l'inspecteur web. Les requêtes de `tauri://localhost` vers
   `http://127.0.0.1` et `ws://` n'ont été exercées que sur WebKitGTK.
6. **Téléchargements dans Tauri.** `export_recording` écrit dans
   `~/Downloads` (`src-tauri/src/main.rs:58`), que macOS protège par une
   demande d'autorisation, puis appelle `open -R` (`main.rs:179-187`). Rien
   de cela n'a tourné sur un Mac, et aucun test ne clique un téléchargement
   dans une fenêtre Tauri pour les types `refined` et `refined_share`.
   Premiers clics à essayer : télécharger un enregistrement, un résultat
   affiné, une archive de partage. Un fichier doit apparaître dans
   Téléchargements et le Finder doit s'ouvrir dessus.
7. **Durée de l'affinage et fils sur le M5.** Un travail prend tous les
   cœurs sauf un (`crates/gs-server/src/refine.rs:270`), cœurs économes
   compris. Comparer avec `--refine-threads` réglé sur le nombre de cœurs
   performants. Le cache des vues fait 1 Go
   (`crates/gs-refine/src/job.rs:131`) ; la mémoire n'a été mesurée que sur
   des captures simulées.
8. **Espace disque libre.** `statvfs` (`crates/gs-server/src/store.rs:199`)
   n'a tourné que sous Linux. Vérifier
   `curl http://127.0.0.1:7878/api/record` et comparer `storage.free_bytes`
   au Finder. Sur APFS l'espace purgeable n'est pas compté : l'alerte sous
   2 Gio et le refus 507 à l'envoi peuvent arriver trop tôt. Le
   comportement sur un disque réellement plein a été vérifié ici, sur un
   volume en mémoire de 300 Mo.
9. **Vue en direct depuis Safari** sur iPhone, iPad ou Vision Pro par le
   réseau local : Safari peut refuser une connexion `wss://` vers un
   certificat auto-signé même après acceptation de l'avertissement de la
   page. Les enregistrements et les résultats affinés passent par de
   simples requêtes HTTPS et sont moins exposés.
10. **Système de fichiers insensible à la casse (APFS).** Les fichiers en
    cours d'écriture sont suivis par leur nom exact
    (`crates/gs-server/src/store.rs:455`) : un `DELETE` qui écrit le nom
    avec une autre casse passe le contrôle « occupé » et supprime un fichier
    en cours d'usage. Atteignable seulement depuis la boucle locale avec une
    origine autorisée ; l'interface utilise toujours le nom listé.
11. **Prérequis de compilation.** L'espace de travail demande Rust 1.87.
    Trunk télécharge wasm-bindgen 0.2.130 et wasm-opt pour macOS au premier
    `trunk build` : cette étape demande le réseau. `bundle.active` vaut
    `false` dans `src-tauri/tauri.conf.json:44` : `cargo tauri build` donne
    un binaire nu, pas un `.app`.
12. **Suites de bout en bout sous macOS.** Voir la section 8. Les contrôles
    sensibles au temps peuvent se comporter autrement sur une machine bien
    plus rapide : la suite `refine` de `e2e/app` a besoin qu'un travail de
    150 itérations dure assez pour être annulé (`e2e/app/refine.cjs`,
    `ITERATIONS`).

## 6. Défauts et limites connus

### Capture réelle

- La capacité est d'un million de cellules. Sur une marche simulée de dix
  minutes dans une maison, elle est atteinte vers la neuvième minute ; au
  delà, les nouvelles surfaces ne sont plus ajoutées. Une vraie maison plus
  grande l'atteindra plus tôt. Piste : une tolérance d'association qui
  croît avec la distance dans `VoxelFusion` (`crates/gs-core/src/reconstruct.rs`).
- La fusion ne retire jamais une surface que des images ultérieures voient
  au travers : une personne qui traverse la pièce pendant la capture laisse
  un fantôme.
- La compensation d'exposition ancre toute la carte sur les premières images.
- Les murs vus de très près (30 à 50 cm) paraissent floconneux.
- Un fichier de capture dont un enregistrement est abîmé est refusé en
  entier à l'import.
- Le jeton d'appairage change à chaque démarrage du serveur : une capture
  enregistrée hors ligne puis envoyée plus tard sera souvent refusée (401) ;
  il faut alors scanner de nouveau le QR code.

### Enregistrement et partage

- Un enregistrement servi par un hébergeur sans requêtes partielles
  (`Range`) est gardé entier en mémoire et coûte environ trois fois sa
  taille (688 Mo ont pris 2,0 Go). Avec `Range`, le cache est borné à 96 Mio.
- Un enregistrement de scan n'a qu'une image clé : chaque retour en arrière
  rejoue le scan jusqu'à l'instant voulu. Pas de lecture à l'envers.
- Le bouton d'enregistrement tourne jusqu'à 8 Gio ou jusqu'au disque plein.
  L'interface avertit sous 2 Gio libres, sans empêcher de démarrer.

### Serveur

- Pas de limite de connexions par adresse sur le réseau local : un appareil
  sans jeton peut occuper l'écoute HTTPS en se reconnectant sans cesse
  (borné par tentative, pas empêché).

### Rendu

- Le mode normalisé scintille un peu plus que le mode trié quand la caméra
  tourne ; le niveau « dur » crénelle les contours.
- Désactiver la soustraction du fond n'est plus temps réel avec les
  réglages par défaut.

### Scène de démonstration

- La marche du scan de maison démarre et s'arrête brusquement et ne couvre
  pas le côté ouest de la chambre.

### Affinage par le serveur

- Le temps restant affiché au début d'un affinage est environ trois fois
  trop court (« reste environ 37 secondes » à l'itération 20 d'un travail de
  109 secondes) : les premiers 30 % tournent à demi-largeur et vont plus
  vite.
- Un résultat tiré d'une capture d'une seule image, ou mal coupée, est
  proposé comme un résultat normal.
- La vue de départ d'une pièce affinée est la position de la première image
  du téléphone, tournée vers le centre du nuage. Une vraie capture qui
  commence face à un mur donnera une mauvaise première vue. Un `.ply`
  produit par la ligne de commande et déposé dans le dossier n'a pas de vue
  enregistrée et s'ouvre au milieu de ses limites.
- Un export de jeu de données ne regarde l'annulation que pendant la
  fusion ; annulé pendant la copie des images, il va au bout puis supprime
  le dossier.
- Un travail qui échoue ou panique rapporte `internal` avec un message en
  anglais seulement.
- Dans le formulaire, le choix « Peindre d'après les photos » est tronqué à
  la largeur par défaut du panneau.
- La zone que le téléphone n'a jamais vue reste une tache sombre dans le
  résultat (visible au premier plan de la capture d'écran du parcours
  complet).

### Captures conservées

- Rouvrir une capture conservée (bouton œil) la réimporte et écrit un
  `.gsrec` de plus à chaque fois.
- Pas de progression ni d'annulation pendant l'import d'une capture par le
  serveur : la page montre une barre indéterminée.
- Le refus d'une capture par un disque plein est dit à la page de bureau
  seulement, pas à l'iPhone ni aux spectateurs du réseau local.
- Sur un disque plein, le bouton d'enregistrement répond 500 `internal` au
  lieu d'un code « disque plein » ; la page est tout de même prévenue par
  `storage.problem`.

### Tests

- Il n'existe aucune passe entièrement verte de `e2e/app` : 499 sur 500 à
  la dernière. Le contrôle `soak` de la mémoire du worker échoue plus
  souvent qu'il ne passe à 40 secondes et passe à `SOAK_S=240` (palier
  atteint tard, pas une fuite). Un contrôle de `shared` après redémarrage
  du serveur compare deux valeurs à un instant d'un scan en cours et a
  échoué une fois. Les deux tests sont laissés tels quels.
- Le comportement de l'interface pour une capture que le téléphone vient de
  terminer (affinage immédiat) n'a pas de test de bout en bout dans le
  dépôt : le serveur a un test Rust, l'interface a été vérifiée une fois à
  la main.
- La branche macOS du repli sans `/proc` (`ps -M` dans
  `e2e/server/abuse.py`) n'a jamais tourné. Les outils Python ont tourné
  avec Python 3.13 ; avec le 3.9 de macOS, seule l'analyse syntaxique a été
  vérifiée.

## 7. Règles du projet

- La spécification `docs/ARCHITECTURE.md` fait foi. Toute décision prise là
  où elle se tait y est écrite.
- Les formats (`GSCF`, `.gscap`, `.gsrec`, messages `GSL1`) ont chacun deux
  implémentations indépendantes (Rust et Python) et des fichiers d'exemple.
  Un changement de format touche les deux, les fichiers d'exemple, et la
  spécification, ou ne se fait pas.
- Méthode qui a fait ses preuves ici : chaque lot est écrit, puis attaqué
  par un vérificateur indépendant à qui l'on demande de réfuter, pas de
  confirmer. Chaque passe a trouvé de vrais défauts, dont un qui aurait
  bloqué l'import de toute capture réelle.
- On n'affaiblit pas un test pour le faire passer. Si une attente doit
  bouger, on dit laquelle et pourquoi.
- On regarde les images. Plusieurs régressions de qualité n'étaient
  visibles que sur les planches de comparaison, pas dans les chiffres.
- Tout ce que lit l'utilisateur est en français ; le code et ses
  commentaires sont en anglais.
- Rapports honnêtes : dire ce qui a été lancé, ce qui a seulement été lu, et
  ce que personne n'a pu vérifier.

## 8. Lancer les suites de bout en bout

Elles demandent Node, Playwright avec Chromium, et pour `e2e/app` et
`e2e/server` un serveur compilé en mode release et l'interface construite.

```bash
(cd e2e && npm install && npx playwright install chromium)   # une fois
node e2e/render/run.cjs
SKIP_BUILD=1 node e2e/app/run.cjs
SKIP_BUILD=1 ONLY=refine node e2e/app/run.cjs                # une seule suite
python3 e2e/server/capture.py
python3 e2e/server/refine.py
python3 e2e/server/hostile.py --quick
python3 e2e/server/abuse.py
node e2e/server/share.cjs
```

`e2e/package.json` apporte Playwright et l'émulateur WebXR IWER pour tous les
scripts ; aucune variable d'environnement n'est nécessaire.

Sur le Mac, ces suites tourneront pour la première fois avec un vrai GPU :
les contrôles de cadence, épinglés au mode trié à cause du rendu logiciel,
pourront être resserrés. `e2e/server/tauri-smoke.sh`, `e2e/server/tauri-real.sh` et la suite `webkit`
de `e2e/app` supposent Xvfb ou WebKitGTK : ils sont propres à Linux (la
suite `webkit` se saute d'elle-même). `hostile.py` et `abuse.py` lisaient
`/proc` ; ils se rabattent maintenant sur `ps` et `lsof`, voie forcée et
vérifiée sous Linux avec `GS_NO_PROC=1`, jamais exécutée sur macOS.
