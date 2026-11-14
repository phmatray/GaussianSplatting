# Splat Live

Splat Live capture une scène en 3D sous forme de « splats » gaussiens et la
montre en temps réel, sur un écran ou dans un casque de réalité virtuelle.

- **Deux démonstrations simulées**, sans aucun matériel : une scène de danse
  filmée par un anneau de caméras de profondeur, et le scan progressif d'une
  maison.
- **La capture réelle avec un iPhone Pro** (scanner LiDAR) : l'application
  compagnon SplatCapture envoie ses images à l'ordinateur, qui reconstruit la
  pièce, l'objet ou la personne pendant que vous filmez.
- **L'enregistrement 4D** : ce que voient les spectateurs est enregistré,
  relu, partagé par un lien ou publié sur un site web.
- **L'affinage hors ligne** : une capture conservée est optimisée contre ses
  propres photos pour gagner en netteté, ou exportée vers d'autres logiciels
  d'entraînement (COLMAP, nerfstudio).

Tout est écrit en Rust (Tauri 2, Leptos, WebGL2, WebXR), sauf l'application
iPhone (Swift, ARKit). La spécification complète est dans
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

> **À lire avant tout** : rien de ce dépôt n'a encore tourné sur du vrai
> matériel. Voir « État et limites » à la fin.

## Ce qu'il faut installer

- Rust 1.87 ou plus récent, avec la cible WebAssembly :
  `rustup target add wasm32-unknown-unknown`
- [Trunk](https://trunkrs.dev) 0.22, qui construit l'interface web :
  `cargo install trunk`
- Pour l'application de bureau : les prérequis de
  [Tauri 2](https://tauri.app/start/prerequisites/) (rien de plus que Xcode
  sous macOS).
- Pour les tests de bout en bout : Node.js, Python 3 et Playwright.

## Construire et lancer

L'interface web se construit une fois, et sert aux trois façons de lancer le
produit :

```bash
cd crates/gs-app && trunk build --release && cd ../..
```

### L'application de bureau

```bash
cargo build --release -p splat-live --features custom-protocol
./target/release/splat-live
```

La fenêtre démarre son propre serveur. Les enregistrements, les captures et
les captures affinées vont dans le dossier de données de l'application
(`~/Library/Application Support/dev.splatlive.desktop` sous macOS). Les
détails, le mode développement et la création d'un paquet d'installation
sont dans [`src-tauri/README.md`](src-tauri/README.md).

### Le serveur seul, dans un navigateur

```bash
cargo run --release -p gs-server -- --dist crates/gs-app/dist
```

Ouvrez ensuite `http://127.0.0.1:7878`. `gs-server --help` liste les options
(port, scène de départ, dossiers, `--light` pour une machine modeste). Sans
option, les fichiers vont dans `~/Library/Application Support/splat-live`
sous macOS (`~/.local/share/splat-live` sous Linux).

### La version web statique, sans serveur

Le dossier `crates/gs-app/dist` se dépose tel quel sur n'importe quel
hébergement web statique. Pour l'essayer :

```bash
python3 -m http.server 8080 --directory crates/gs-app/dist
```

puis `http://127.0.0.1:8080`. Le calcul tourne alors dans le navigateur (un
Web Worker en WebAssembly), plus lentement. Les deux démonstrations, la
relecture d'un enregistrement et l'import d'un fichier de capture `.gscap`
fonctionnent ; la capture en direct, l'enregistrement sur disque et
l'affinage demandent le serveur.

## Les deux démonstrations

Les boutons du haut de la fenêtre passent de l'une à l'autre.

- **Scène en direct** : huit caméras de profondeur simulées entourent une
  danseuse. À chaque instant, leurs images deviennent un nuage de splats que
  vous regardez de n'importe où (souris : tourner et zoomer).
- **Scan de maison** : une tête de cinq capteurs parcourt une maison
  meublée. La maquette se construit pièce après pièce, vue de dessus ; une
  fois le scan terminé, vous la visitez de l'intérieur (touches ZQSD d'un
  clavier français, WASD d'un clavier anglais, et la souris).

Le panneau de droite règle le nombre de caméras, leur résolution, le rendu
et l'affichage. H masque l'interface, P met en pause, R enregistre.

## Le casque VR sur le réseau local

Le casque n'a rien à installer : il ouvre une page servie par l'ordinateur.

1. Lancez le serveur avec l'accès réseau : `--lan` en ligne de commande, ou
   l'interrupteur de la section « Casque VR » du panneau.
2. Le serveur affiche une adresse `https://192.168.x.x:7879`, son QR code et
   l'empreinte SHA-256 de son certificat.
3. Dans le navigateur du casque, ouvrez cette adresse. Le certificat est
   auto-signé (WebXR exige HTTPS) : le navigateur affiche un avertissement.
   Comparez l'empreinte qu'il montre avec celle de l'ordinateur, puis
   acceptez.
4. Appuyez sur « Entrer en VR ». Le stick gauche déplace, le stick droit
   tourne par crans, un bouton bascule entre la taille réelle et une
   maquette posée devant vous.

L'accès réseau n'a pas de mot de passe : toute personne du même réseau peut
regarder. Il est coupé par défaut.

## Capturer avec un iPhone

Il faut un iPhone Pro ou un iPad Pro avec scanner LiDAR (iOS 17 ou plus
récent) et un Mac avec Xcode 15 ou plus récent.

### Installer l'application compagnon

```bash
brew install xcodegen
cd ios/SplatCapture
xcodegen generate
open SplatCapture.xcodeproj
```

Dans Xcode, choisissez votre équipe de signature (onglet « Signing &
Capabilities »), branchez l'iPhone, sélectionnez-le comme destination et
lancez (Cmd-R). Le pas à pas complet, avec le mode développeur de l'iPhone
et les limites du compte Apple gratuit, est dans
[`ios/SplatCapture/README.md`](ios/SplatCapture/README.md).

### Jumeler le téléphone et l'ordinateur

1. Sur l'ordinateur, section « iPhone » du panneau : « Afficher le code
   d'appairage » (l'accès réseau s'active tout seul).
2. Sur l'iPhone, onglet Réglages : « Scanner le QR code du serveur ».

Le code contient l'adresse de l'ordinateur, l'empreinte de son certificat
(l'application n'en acceptera aucun autre) et un jeton qui change à chaque
démarrage du serveur : il faut le scanner de nouveau après un redémarrage.

### Les quatre modes

| Mode | Usage |
|---|---|
| Pièce | parcourir une ou plusieurs pièces ; portée de 5 m |
| Objet | tourner autour d'un objet immobile ; le reste est écarté |
| Personne | tourner autour d'une personne immobile, reconstruite seule |
| Direct | chaque image remplace la précédente : un relief vu du téléphone |

Pendant que vous filmez, la reconstruction apparaît sur l'ordinateur et dans
les casques connectés. À la fin, la capture est enregistrée toute seule
(section « Enregistrement ») et son flux brut est conservé (section
« Captures et affinage »).

### Sans réseau : enregistrer puis importer

Hors de portée de l'ordinateur, l'application enregistre un fichier `.gscap`.
Transférez-le ensuite (AirDrop, app Fichiers) et ouvrez-le depuis la section
« iPhone » du panneau (« Importer un fichier .gscap »), ou glissez-le dans
la fenêtre. La version web statique sait aussi le reconstruire, sans
serveur.

### Essayer sans iPhone

Un téléphone simulé joue une capture synthétique contre le serveur :

```bash
cargo run --release -p gs-server -- --lan --dist crates/gs-app/dist
# dans un autre terminal
python3 tools/gscap.py synth /tmp/marche.gscap --frames 60 --name Marche
URI=$(curl -s http://127.0.0.1:7878/api/pair | python3 -c 'import sys,json; print(json.load(sys.stdin)["uri"])')
python3 tools/fake_phone.py stream /tmp/marche.gscap "$URI" --realtime
```

Une capture plus réaliste (la chambre de la maison simulée, filmée par un
téléphone simulé) s'écrit avec :

```bash
cargo run --release -p gs-core --features par --example gscap_write -- bedroom /tmp/chambre.gscap
```

## Enregistrer, relire, partager

- **Enregistrer** : le bouton rouge (ou R) enregistre ce que voient les
  spectateurs dans un fichier `.gsrec`. Un scan terminé s'enregistre d'un
  clic, comme une image fixe.
- **Relire** : la section « Enregistrement » liste les fichiers. La ligne de
  temps permet de lire, mettre en pause, changer de vitesse, boucler et se
  déplacer (K ou P, J et L, virgule et point). Dans un casque : Y à gauche
  pour lire, gâchette latérale gauche et stick pour se déplacer dans le
  temps.
- **Partager** (le bouton d'envoi d'un enregistrement) :
  - un **lien** sur le réseau local, avec son QR code, à ouvrir sur un
    téléphone ou dans un casque ;
  - un **code d'intégration** `<iframe>` pour une page web ;
  - une **archive web** `.zip` : la visionneuse et l'enregistrement, à
    décompresser sur n'importe quel hébergement statique.

Un fichier `.gsrec`, `.ply` ou `.splat` s'ouvre aussi par un lien :
`https://exemple.org/visionneuse/?file=danse.gsrec&autoplay=1&loop=1`
(paramètres : `file`, `embed`, `autoplay`, `loop`, `t`, `view`, `nav`).

## Affiner une capture

La reconstruction en direct ne compare jamais son image aux photos.
L'affinage reprend une capture conservée et optimise des gaussiennes contre
ses photos (3D Gaussian Splatting, avec la profondeur du LiDAR comme guide).

**Ce qu'il faut en attendre**, mesuré sur des captures simulées seulement :
des contours plus nets et un net gain près du trajet du téléphone (plusieurs
décibels depuis ses propres points de vue) ; peu ou rien loin du trajet, là
où la reconstruction est déjà fermée ; un léger moutonnement sur les
surfaces unies ; les zones jamais vues restent des trous. Le calcul
n'utilise que le processeur : une version GPU est prévue pour le Mac et
n'existe pas encore.

### Depuis l'application

Section « Captures et affinage » du panneau (avec le serveur ou
l'application de bureau, sur l'ordinateur lui-même) :

1. chaque capture conservée se rouvre (l'œil), s'affine (l'étoile) ou se
   supprime ; une capture que le téléphone vient de terminer s'affine tout
   de suite, mais ne se supprime qu'une fois quittée (revenez à une
   démonstration avec les boutons du haut) ;
2. « Affiner cette capture » demande un préréglage et ce qu'il faut faire là
   où le capteur n'a rien mesuré (peindre d'après les photos, ou laisser les
   trous) ;
3. la progression s'affiche ; « Annuler » abandonne le calcul sans rien
   garder ;
4. à la fin, « Voir le résultat » ouvre le fichier `.ply` dans la
   visionneuse ; il se télécharge et se partage comme un enregistrement.

| Préréglage | Images | Itérations au moins | Durée mesurée (pièce simulée de 220 photos, machine virtuelle à 4 cœurs) |
|---|---|---|---|
| Rapide | 480 px | 1 500 | environ 2 minutes |
| Standard | 640 px | 4 000 | environ 10 minutes |
| Fin | 960 px | 12 000 | jamais mené à son terme : plus d'une heure |

Une vraie capture est plus longue et ses photos sont plus grandes : comptez
davantage.

### En ligne de commande

```bash
cargo build --release -p gs-refine
target/release/gs-refine info /tmp/chambre.gscap
target/release/gs-refine /tmp/chambre.gscap -o /tmp/chambre.ply --preset rapide
```

Le fichier `chambre.ply` s'ouvre dans la visionneuse (section « Fichier »,
ou glissé dans la fenêtre) ; `chambre.json` contient le rapport. Ctrl-C
arrête proprement et `--reprise /tmp/chambre.reprise` reprend. `--mesure-seule`
laisse en trous ce que le capteur n'a pas mesuré. `gs-refine aide` décrit
toutes les options.

Le score que l'outil affiche à la fin (« vues mises de côté ») est mesuré sur
des photos voisines de celles de l'entraînement : il ne dit pas la qualité
vue d'ailleurs.

### Exporter vers d'autres logiciels d'entraînement

```bash
target/release/gs-refine export /tmp/chambre.gscap --format colmap -o /tmp/chambre_colmap
target/release/gs-refine export /tmp/chambre.gscap --format nerfstudio -o /tmp/chambre_ns
```

Les mêmes exports sont proposés dans l'application, sous « Affiner cette
capture ». Le jeu de données contient les photos, les poses, les cartes de
profondeur (nerfstudio) et le nuage de départ. Pour nerfstudio, gardez le
repère de la capture : `--orientation-method none --center-method none
--auto-scale-poses False`. Le `.ply` qu'un autre logiciel produit s'ouvre
dans la visionneuse.

## Les tests

```bash
# Rust : toutes les bibliothèques (plusieurs minutes)
cargo test --no-fail-fast
# la coquille de bureau, à part (demande les bibliothèques de Tauri)
cargo test -p splat-live

# les outils de référence en Python (formats de fichiers, téléphone simulé)
python3 -m unittest discover -s tools

# la bibliothèque de l'application iPhone, sur un Mac (sans Xcode ni iPhone)
cd ios/SplatCapture/Packages/SplatCaptureKit && swift test
```

Les vérifications de bout en bout pilotent un navigateur sans écran. Elles
demandent l'interface construite, le serveur compilé
(`cargo build --release -p gs-server`) et Playwright :

```bash
(cd e2e && npm install && npx playwright install chromium)   # une fois : Playwright, son Chromium et l'émulateur WebXR
SKIP_BUILD=1 node e2e/app/run.cjs       # l'application : démonstrations, relecture, capture, affinage, VR émulée
SKIP_BUILD=1 ONLY=refine node e2e/app/run.cjs   # une seule suite
node e2e/render/run.cjs                 # le moteur de rendu comparé à sa référence
python3 e2e/server/capture.py           # le serveur face au téléphone simulé
python3 e2e/server/refine.py            # l'affinage et les exports par le serveur
python3 e2e/server/hostile.py --quick   # le serveur face à un client malveillant
node e2e/server/share.cjs               # le partage de bout en bout
```

Chaque dossier (`e2e/app`, `e2e/render`, `e2e/server`) a son `README.md`.

## État et limites

Ce dépôt a été développé et vérifié sur une machine virtuelle Linux, sans
écran, sans carte graphique, sans Mac, sans iPhone et sans casque.

**Ce qui n'a jamais tourné**

- **macOS** : ni le serveur, ni l'application de bureau n'y ont été
  compilés. La coquille Tauri a été lancée sous Linux seulement.
- **Xcode et l'iPhone** : l'application SplatCapture n'a jamais été
  compilée. Aucune image d'un vrai iPhone n'a traversé le pipeline ; tout a
  été testé avec un téléphone simulé.
- **Une vraie carte graphique** : le rendu a été vérifié avec un moteur
  logiciel (SwiftShader). Les débits d'images sur un vrai GPU ne sont pas
  connus.
- **Un vrai casque** : la réalité virtuelle a été vérifiée avec un émulateur
  WebXR, jamais dans un casque.
- **Safari et Firefox** : seul Chromium a été utilisé.

**Ce que valent les mesures**

- Toutes les mesures de qualité (reconstruction en direct, affinage) portent
  sur des scènes simulées, avec un bruit de capteur imaginé d'après la
  documentation. Un vrai LiDAR se trompe autrement.
- Les réglages de l'affinage ont été choisis sur une chambre simulée. Sur
  une vraie pièce, les deux premiers essais à faire sont l'option « laisser
  les trous » (`--mesure-seule`) et `--set train.loss.ssim=0`.
- Les durées viennent d'une machine virtuelle à 4 cœurs.

**Limites connues**

- La reconstruction en direct garde un million de cellules : au-delà (une
  grande maison), les nouvelles surfaces ne sont plus ajoutées, et
  l'interface le dit.
- Pas de recalage des poses ni de fermeture de boucle : une dérive d'ARKit
  dédouble les murs.
- Les surfaces qui changent pendant la capture (une personne qui passe)
  laissent des fantômes.
- Couleur indépendante du point de vue : les reflets sont moyennés.
- Un enregistrement servi par un hébergement sans requêtes partielles
  (`Range`) est chargé en entier en mémoire.

La marche à suivre pour la première mise en route sur le Mac est dans
[`docs/HANDOFF.md`](docs/HANDOFF.md).
