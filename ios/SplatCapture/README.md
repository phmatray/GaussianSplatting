# SplatCapture : l'application compagnon pour iPhone et iPad

SplatCapture filme une pièce, un objet ou une personne avec la caméra et le
scanner LiDAR d'un iPhone Pro ou d'un iPad Pro, et envoie chaque image
(couleur, profondeur, position de la caméra donnée par ARKit) au serveur
Splat Live qui tourne sur votre ordinateur. Le serveur en fait un nuage de
splats que vous regardez sur l'ordinateur ou dans un casque.

Sans serveur à portée, l'application enregistre les mêmes images dans un
fichier `.gscap` que vous transférez ensuite (AirDrop, app Fichiers, ou envoi
direct au serveur).

Le format des images et du fichier est décrit dans
`docs/ARCHITECTURE.md`, section « Real capture: the phone companion ».

## Ce qu'il vous faut

- Un Mac avec Xcode 15 ou plus récent (gratuit sur le Mac App Store).
- Un iPhone Pro ou Pro Max à partir de l'iPhone 12 Pro, ou un iPad Pro à
  partir de 2020, sous iOS ou iPadOS 17 ou plus récent. Sans scanner LiDAR,
  l'application s'installe mais l'onglet Capture affiche seulement un message.
- Un câble USB pour la première installation.
- Un identifiant Apple. Le compte gratuit suffit (voir ses limites plus bas).
- Homebrew (https://brew.sh) pour installer XcodeGen.
- Le Mac et l'iPhone sur le même réseau Wi-Fi.

## De zéro à l'application sur le téléphone

1. Installez XcodeGen, qui fabrique le projet Xcode à partir du fichier
   `project.yml` :

   ```sh
   brew install xcodegen
   ```

2. Dans le dépôt, générez le projet puis ouvrez-le :

   ```sh
   cd ios/SplatCapture
   xcodegen generate
   open SplatCapture.xcodeproj
   ```

   Le dossier `SplatCapture.xcodeproj` est produit par cette commande et n'est
   pas versionné. Relancez `xcodegen generate` après chaque mise à jour du
   dépôt.

3. Indiquez votre équipe de développement. Dans Xcode : Settings > Accounts,
   ajoutez votre identifiant Apple s'il n'y est pas. Puis cliquez sur le
   projet SplatCapture (en haut de la colonne de gauche), sur la cible
   SplatCapture, onglet « Signing & Capabilities », et choisissez votre nom
   dans le menu « Team » (avec un compte gratuit : « Votre Nom (Personal
   Team) »).

   Ce choix est effacé à chaque `xcodegen generate`. Pour le rendre
   définitif, écrivez votre identifiant d'équipe (10 caractères, visible dans
   Xcode après avoir choisi l'équipe : Build Settings, chercher
   « Development Team ») dans `project.yml`, à la ligne
   `DEVELOPMENT_TEAM: ""`.

   Si Xcode indique que l'identifiant `be.splatlive.capture` n'est pas
   disponible, remplacez-le dans `project.yml` par un identifiant à vous, par
   exemple `be.votrenom.splatcapture`, et régénérez le projet.

4. Branchez l'iPhone au Mac avec le câble, déverrouillez-le et acceptez
   « Faire confiance à cet ordinateur ».

5. Activez le mode développeur sur l'iPhone : Réglages > Confidentialité et
   sécurité > Mode développeur (tout en bas), puis redémarrez quand il le
   demande. Si la ligne n'apparaît pas, lancez une première fois l'étape 6 :
   Xcode la fait apparaître.

6. Dans Xcode, en haut de la fenêtre, choisissez le schéma « SplatCapture »
   et votre iPhone comme destination (pas un simulateur : il n'a ni caméra ni
   LiDAR). Lancez avec le bouton lecture ou Cmd-R. La première compilation
   prend une à deux minutes.

7. Au premier lancement, l'iPhone refuse d'ouvrir l'application d'un
   développeur qu'il ne connaît pas. Sur l'iPhone : Réglages > Général > VPN
   et gestion de l'appareil, touchez votre identifiant Apple sous
   « App du développeur », puis « Faire confiance ». Relancez depuis Xcode.

8. Acceptez les deux autorisations que l'application demande : la caméra
   (au premier affichage de l'onglet Capture) et le réseau local (au premier
   contact avec le serveur).

Ensuite l'application se lance depuis l'écran d'accueil, sans câble ni Xcode.

### Limites du compte Apple gratuit

- L'application installée cesse de s'ouvrir au bout de 7 jours. Il suffit de
  la relancer depuis Xcode (Cmd-R) pour repartir pour 7 jours. Vos
  enregistrements sont conservés.
- Trois applications de ce type au plus par appareil, et dix identifiants
  d'application nouveaux par période de 7 jours.
- Le compte payant (Apple Developer Program) porte la durée à un an.

## Jumeler l'application avec le serveur

Le serveur chiffre la liaison avec un certificat qu'il a fabriqué lui-même.
Le jumelage apprend à l'application l'adresse du serveur, l'empreinte de ce
certificat (elle n'en acceptera aucun autre) et un jeton d'accès.

1. Sur l'ordinateur, lancez Splat Live, activez l'accès par le réseau local,
   puis affichez le QR code de jumelage.
2. Sur l'iPhone, onglet Réglages, touchez « Scanner le QR code du serveur »
   et visez l'écran.
3. La ligne d'état doit indiquer « Jumelé avec 192.168.x.x : joignable ».

Si le QR code ne peut pas être scanné, copiez l'adresse de jumelage (elle
commence par `splatlive://pair?`) et collez-la dans « Saisie manuelle ». Avec
le presse-papiers universel d'Apple, un texte copié sur le Mac se colle
directement sur l'iPhone.

Le jeton change à chaque démarrage du serveur : il faut alors scanner le QR
code de nouveau. Le jumelage est conservé d'un lancement à l'autre de
l'application.

## Filmer

Onglet Capture : choisissez un mode, donnez un nom, touchez Démarrer. Pause
suspend l'envoi, Terminer clôt la capture, Réinitialiser efface ce qui a été
capturé et repart de la position actuelle.

L'encadré du haut indique l'état de la liaison, des conseils quand le suivi
de la position faiblit (« Bougez plus lentement »), et trois compteurs :
images envoyées, images confirmées par le serveur, images ignorées (celles
que le réseau ou le serveur n'a pas eu le temps d'absorber : quelques-unes
sont normales, beaucoup signifient que le Wi-Fi est trop lent, réduisez alors
la taille de l'image dans Réglages).

### Les quatre modes

| Mode | Pour quoi | Comment filmer |
|---|---|---|
| Pièce | Une pièce, un étage, une maison | Marchez lentement, téléphone à hauteur de poitrine, en balayant murs, sol et plafond |
| Objet | Un objet posé, de la taille d'une chaussure à celle d'un fauteuil | Touchez l'objet à l'écran, réglez le rayon, puis tournez autour de lui |
| Personne | Une personne qui reste immobile | Touchez la personne à l'écran, réglez le rayon (environ 1,2 m), puis faites le tour |
| Direct | Voir en direct ce que voit le téléphone | Rien n'est accumulé : chaque image remplace la précédente, c'est un relief vu depuis le téléphone |

En mode Objet et Personne, la sphère jaune montre la zone que le serveur
garde ; ce qui est en dehors est ignoré. Touchez de nouveau l'écran pour la
déplacer.

### Conseils pratiques

- Distance : le LiDAR porte à environ 5 m et il est le plus précis entre
  0,5 m et 3 m. Dans une grande pièce, approchez-vous des murs au lieu de
  rester au centre. Pour un objet, restez entre 40 cm et 1 m.
- Vitesse : lentement. Un pas par seconde en marchant, un tour complet
  d'objet en 30 à 60 secondes. Les mouvements brusques rendent les images
  floues et font décrocher le suivi.
- Recouvrement : repassez sur des zones déjà filmées quand vous changez de
  direction, et terminez un tour là où vous l'avez commencé.
- Lumière : une lumière abondante et régulière. Le LiDAR fonctionne dans le
  noir mais pas la caméra, et le suivi de position a besoin de la caméra.
  Évitez le contre-jour devant une fenêtre.
- Surfaces difficiles : miroirs, vitres, écrans, métal poli et surfaces
  noires mates renvoient mal le laser. Attendez-vous à des trous ou à des
  fantômes derrière les miroirs. Un mur blanc uni sans aucun détail peut faire
  décrocher le suivi : gardez un meuble ou un angle dans le champ.
- Petits objets : la carte de profondeur du LiDAR est grossière (256 x 192
  points). En dessous d'une dizaine de centimètres, les formes sont
  approximatives. Posez l'objet sur une surface texturée (un journal) et
  éloignez-le des murs.
- Personne : elle doit rester immobile, bras légèrement écartés du corps,
  pendant tout le tour. Faites deux tours : un à hauteur de poitrine, un plus
  haut en visant vers le bas.
- Sol : gardez un bout de sol dans le champ au début. L'application signale
  au serveur la hauteur du sol dès qu'ARKit l'a reconnu.
- L'option « Afficher le maillage de la scène » (Réglages) dessine ce que le
  LiDAR a déjà couvert : pratique pour repérer les oublis.
- Tenue du téléphone : portrait ou paysage, peu importe, mais ne masquez pas
  le LiDAR (le rond noir à côté des objectifs) avec un doigt.

## Enregistrer sans serveur

Activez « Enregistrer sans serveur » avant de toucher Démarrer. Les images
vont dans un fichier `.gscap` du dossier Documents de l'application (compter
de 100 à 400 Mo par minute en mouvement, selon la taille d'image choisie).
C'est aussi ce qui se passe tout seul si le serveur ne répond pas au moment
de démarrer : l'application attend 8 secondes au plus, puis enregistre sur
l'appareil et l'indique dans l'encadré du haut.

Le fichier est écrit image après image. Si l'application est fermée ou que
la batterie lâche en cours de route, tout ce qui a été filmé jusque-là reste
lisible : l'enregistrement apparaît dans la liste au lancement suivant, avec
la mention qu'il a été interrompu si sa dernière image est incomplète.

L'onglet Enregistrements liste les fichiers (nom, date, taille, nombre
d'images). Pour chacun :

- « Partager » ouvre la feuille de partage : AirDrop vers le Mac, ou
  « Enregistrer dans Fichiers ». Sur le Mac, ouvrez ensuite le fichier dans
  Splat Live.
- « Envoyer au serveur » le transmet directement au serveur jumelé.
- « Supprimer » (ou un balayage vers la gauche dans la liste).

Les fichiers sont aussi visibles dans l'app Fichiers (Sur mon iPhone >
SplatCapture) et dans le Finder quand l'iPhone est branché au Mac.

## Essayer sans le vrai serveur

Le dossier `tools/` du dépôt contient un faux serveur en Python (aucune
installation, Python 3 suffit) qui parle exactement le protocole du vrai :
jumelage, certificat, accusés de réception. Il ne reconstruit rien, mais il
vérifie chaque image reçue et peut tout enregistrer. C'est le moyen de
contrôler l'application avant que le serveur Splat Live sache recevoir un
téléphone.

Sur le Mac, depuis la racine du dépôt :

```sh
python3 tools/fake_server.py --save out/captures
```

Il affiche une adresse de jumelage `splatlive://pair?...`. Copiez-la sur le
Mac et collez-la sur l'iPhone dans Réglages > Saisie manuelle. Avec
`brew install qrencode`, le faux serveur affiche aussi un QR code à scanner
dans le terminal. macOS peut demander d'autoriser Python à accepter des
connexions entrantes : acceptez.

Filmez quelques secondes puis touchez Terminer. Le terminal du Mac montre
les messages reçus (`hello`, `floor`, `region`, les images). Ensuite :

```sh
python3 tools/gscap.py verify out/captures/ingest-1.gscap
python3 tools/gscap.py extract out/captures/ingest-1.gscap out/images --every 10
```

La première commande contrôle chaque image (elle doit finir par `OK`), la
seconde écrit, pour une image sur dix, la photo (`.jpg`), la carte de
profondeur (`_depth_view.png` : clair = proche, noir = pas de mesure), la
confiance, le masque et la pose (`.json`).

Pour éprouver le contrôle de débit, ralentissez le faux serveur : avec
`--ack-delay 0.3 --max-inflight 2`, le compteur d'images ignorées doit monter
et le faux serveur ne doit jamais signaler `flow_control`.

« Envoyer au serveur » fonctionne aussi avec le faux serveur : le fichier
reçu est vérifié et copié dans `out/captures/upload-1.gscap`.

Dans l'autre sens, `tools/fake_phone.py` rejoue un fichier `.gscap` vers un
serveur comme le ferait le téléphone (utile pour mettre au point le vrai
serveur sans sortir l'iPhone) :

```sh
python3 tools/fake_phone.py stream out/captures/ingest-1.gscap "splatlive://pair?host=..."
```

## Tests

La logique qui ne dépend ni d'ARKit ni de l'écran (encodage des images,
fichier `.gscap`, adresse de jumelage, choix des images, contrôle de débit)
vit dans le paquet Swift `Packages/SplatCaptureKit`, avec des tests qui
relisent les fichiers du dossier `Fixtures/`, les mêmes que ceux de
l'implémentation de référence en Python et du serveur. Ces fichiers sont
embarqués avec les tests (ressource du paquet), qui tournent donc aussi bien
sur le Mac que dans un simulateur ou sur l'iPhone.

Le plus simple, sur le Mac, sans ouvrir Xcode :

```sh
cd ios/SplatCapture/Packages/SplatCaptureKit
swift test
```

Dans Xcode : schéma SplatCapture, un simulateur d'iPhone (ou l'iPhone)
comme destination, puis Product > Test (Cmd-U).

L'un des tests écrit un petit fichier compressé par l'iPhone (ou le Mac) et
affiche la commande pour le faire contrôler par la référence Python :

```sh
python3 tools/gscap.py verify "$TMPDIR/splatcapture-swift-sample.gscap"
```

Les outils Python ont leurs propres tests :
`python3 -m unittest discover -s tools`.

## En cas de problème

| Symptôme | Cause probable et remède |
|---|---|
| Xcode : « Signing for SplatCapture requires a development team » | Choisissez l'équipe (étape 3) |
| Xcode : « Failed to register bundle identifier » | L'identifiant est pris : changez `PRODUCT_BUNDLE_IDENTIFIER` dans `project.yml`, puis `xcodegen generate` |
| Xcode : « Developer Mode disabled » | Étape 5 |
| L'application ne s'ouvre plus (« n'est plus disponible ») | Les 7 jours du compte gratuit sont écoulés : relancez depuis Xcode |
| « Développeur non approuvé » sur l'iPhone | Étape 7 |
| « Capteur LiDAR introuvable » | L'appareil n'a pas de LiDAR (modèles non Pro) |
| « Serveur injoignable, enregistrement sur l'appareil » | Le serveur n'a pas répondu dans les 8 secondes : la capture continue dans un fichier (onglet Enregistrements), que vous pourrez envoyer au serveur ensuite |
| « Écriture impossible : le stockage est sans doute plein » | Libérez de la place (supprimez d'anciens enregistrements), puis recommencez ; ce qui a été écrit reste lisible |
| « Jumelé avec ... : injoignable » | Vérifiez que les deux appareils sont sur le même Wi-Fi (pas le réseau invité), que le serveur est lancé avec l'accès au réseau local, et que le pare-feu du Mac laisse passer l'application (Réglages Système > Réseau > Coupe-feu) |
| « L'accès au réseau local est refusé » | iPhone : Réglages > Confidentialité et sécurité > Réseau local, activez SplatCapture |
| « Le certificat du serveur ne correspond pas au jumelage » | Le serveur a changé de certificat (ou ce n'est pas le bon serveur) : scannez de nouveau le QR code |
| « Le serveur refuse le jeton de jumelage » | Le serveur a redémarré : scannez de nouveau le QR code |
| Beaucoup d'images ignorées | Wi-Fi trop lent ou serveur trop chargé : rapprochez-vous de la borne, choisissez 640 x 480 dans Réglages |
| « Bougez plus lentement », « Pas assez de détails » | Suivi ARKit limité : ralentissez, éclairez, visez une zone avec du relief |
| L'image de la caméra se fige au retour dans l'application | Touchez Reprendre ; si le suivi ne revient pas, Réinitialiser |
| L'iPhone chauffe et l'image saccade | Pause de quelques minutes ; réduisez la taille de l'image, désactivez le maillage |
| La caméra reste noire | Réglages > SplatCapture > Appareil photo doit être activé |

## Organisation du code

```
ios/SplatCapture/
  project.yml                 description du projet pour XcodeGen
  Support/Info.plist          autorisations et réglages de l'application
  App/                        l'application (SwiftUI, ARKit)
    SplatCaptureApp.swift     point d'entrée, les trois onglets
    AppModel.swift            réglages, jumelage, liste des enregistrements
    CaptureEngine.swift       session ARKit, choix et copie des images
    FrameEncoder.swift        JPEG, profondeur, compression (file d'attente dédiée)
    ServerLink.swift          WebSocket /ingest, certificat épinglé, envoi de fichier
    CaptureView.swift         écran de capture, aperçu ARKit, maillage, zone
    SettingsView.swift        jumelage et réglages
    QRScannerView.swift       lecture du QR code
    RecordingsView.swift      liste, partage, envoi, suppression
  Packages/SplatCaptureKit/   logique sans ARKit ni UIKit, avec ses tests
  Fixtures                    lien vers les données de test partagées, rangées
                              dans Packages/SplatCaptureKit/Tests/SplatCaptureKitTests/Fixtures
                              (voir le README de ce dossier)
```
