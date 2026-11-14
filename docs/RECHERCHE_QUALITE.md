# Recherche : améliorer la fidélité visuelle de la reconstruction en direct

Rapport de recherche du 14 novembre 2026. Question posée : quelles techniques récentes de la
littérature (arXiv d'abord) amélioreraient le plus la fidélité visuelle de Splat Live, compte tenu
de nos contraintes (temps réel, CPU et WASM, pas de CUDA, pas de poids appris, 16 octets par splat,
visionneuse WebGL2 à 72-90 Hz dans un casque autonome) ?

Portée de la vérification. Les titres, premiers auteurs et dates de tous les articles arXiv cités
ont été relus sur leur page arXiv le jour de la rédaction. Les sections méthode, les tableaux de
temps et les limites ont été lus dans le texte intégral pour les articles marqués [L] dans les
références; les autres n'ont été lus qu'au niveau du résumé [R]. Les articles classiques hors arXiv
sont cités d'après des extraits de recherche et la bibliographie d'articles lus [C]. Aucune mesure
n'a été faite sur notre code pour ce rapport : tous les gains annoncés pour Splat Live sont des
prévisions à confirmer par le plan de la section 5. Les coûts "pour nous" sont des estimations,
les chiffres "mesurés" sont ceux des articles, sur leur matériel.

## 1. Synthèse

Réponse courte. Notre reconstruction est déjà, à peu de chose près, ce que les méthodes
"feed-forward" à gaussiennes alignées sur les pixels calculent par réseau : une gaussienne plate
par pixel de profondeur, orientée par le voisinage, colorée par le pixel source. La littérature
récente confirme ce choix (GPS-Gaussian+ prend la couleur du pixel source telle quelle, SurfSplat
déduit rotation et échelle des positions voisines, comme le fait `reconstruct.rs`). Ce qui nous
sépare d'un rendu fidèle n'est donc pas l'absence de réseau ou d'optimisation, mais quatre choses
(le classement par gain rapporté à l'effort est dans le tableau qui suit) :

1. **La règle de composition.** Nous rendons des surfels opaques issus de capteurs avec le mélange
   alpha "over" trié, conçu pour des gaussiennes optimisées. Sans optimisation, ce mélange donne
   l'aspect pointillé, le mélange arbitraire des textures de plusieurs caméras et le coût du tri.
   La bonne règle pour des surfels est le mélange normalisé avec test de profondeur (splatting de
   surface, Zwicker 2001, Botsch 2005), que des systèmes récents reprennent tels quels (Gaussian-
   Plus-SDF SLAM 2025 : couleurs "accumulées et normalisées par les poids", rendu "entièrement
   sans tri").
2. **La concaténation des vues.** Toutes les méthodes sérieuses fusionnent ou sélectionnent
   (GPS-Gaussian+ ne rend que les deux vues voisines de la vue cible, FreeSplat fusionne les
   gaussiennes redondantes par reprojection). Nous empilons huit nuages.
3. **La quantité d'information.** Nous regardons la scène 2 à 8 fois plus finement que les
   capteurs ne l'échantillonnent. Aucune technique de rendu n'invente ce détail. Les seules voies
   sont : couleur à plus haute résolution que la profondeur (c'est le cas de tous les appareils
   RGB-D réels), profondeur suréchantillonnée guidée par la couleur, et détail porté par des
   textures par splat plutôt que par le nombre de splats.
4. **La robustesse des bords.** Silhouettes, pixels volants, normales douteuses aux angles
   rasants : les réseaux y répondent par une carte d'opacité apprise (GPS-Gaussian+), la fusion
   classique par une confiance et un statut stable ou instable (Keller 2013, repris par RTG-SLAM).
   Nous avons une opacité constante.

L'optimisation en ligne des gaussiennes n'est pas la réponse à court terme pour la couche vivante :
les meilleures méthodes de flux publiées demandent de 2 à 12 secondes par image sur GPU de bureau
(3DGStream sur RTX 3090, HiCoM, Instant Gaussian Stream). Elle reste intéressante pour les couches statiques
(plateau vide, maison), en tâche de fond.

Tableau classé par gain attendu rapporté à l'effort et au risque. Le plus fort gain potentiel est
celui du rang 3, mais il ne tient dans nos budgets qu'après le rang 1, et pour la maison qu'avec
le rang 6.

| Rang | Technique | Défauts visés | Gain attendu | Coût à l'exécution (estimé pour nous) | Effort | Risque | Sources |
|---|---|---|---|---|---|---|---|
| 1 | Fusion multi-vues : filtre de cohérence et choix de la meilleure vue par point de surface (A1) | Textures dédoublées entre caméras, "fourrure" rasante, pixels volants | Net sur silhouettes et textures de la scène; 40 à 60 % de splats en moins dans les zones vues par plusieurs caméras (estimation) | 9 000 splats x 7 reprojections : moins de 0,3 ms par tick | Faible | Faible (coutures entre caméras) | FreeSplat, GPS-Gaussian+, Buehler 2001, Keller 2013 |
| 2 | Mélange normalisé avec pré-passe de profondeur, sans tri (C1) | Aspect pointillé, mélange dépendant de l'ordre, popping, surcoût des couches cachées, tri CPU | Fort sur l'aspect des surfaces; supprime le tri des splats | Deux passes géométrie plus une passe plein écran; les couches cachées ne sont plus ombrées | Moyen à élevé (shader, rasteriseur de référence, tests) | Moyen (débit de remplissage du casque, cible flottante) | Zwicker 2001, Botsch 2005, Gaussian-Plus-SDF SLAM, TeSO |
| 3 | Couleur à plus haute résolution que la profondeur, profondeur suréchantillonnée guidée (A2) | Visages et motifs étalés, silhouettes épaissies, structures fines | Le seul levier qui ajoute de l'information : jusqu'à +3 dB sur la part d'erreur due au flou par doublement de résolution (modèle de la section 2) | Reconstruction linéaire en pixels couleur (x4 à résolution double); capture simulée x4 | Moyen | Moyen (budget de splats et de débit, recalage couleur-profondeur réel) | GPS-Gaussian+, Charisoudis 2025, Kopf 2007, He 2013 |
| 4 | Splats plans exacts en perspective (quad en espace monde) et filtre mip normalisé (C2) | Aiguilles aux angles rasants, déformation des gros splats de près (VR), épaississement des fins | Moyen, surtout de près et en rasant; donne la vraie profondeur par fragment (requis par 2 et 6) | Nul à négatif (plus de projection de covariance) | Faible à moyen | Faible | 2DGS, Zwicker 2004, Mip-Splatting, AA-2DGS |
| 5 | Fusion du scan par confiance : moyenne pondérée, statut stable, forme par moments, creusement de l'espace libre (A3) | Fourrure aux arêtes des meubles, structures fines épaissies, cellules aberrantes | Moyen sur les bords; peu sur les aplats | Quelques flottants de plus par cellule, coût négligeable | Moyen | Faible | Keller 2013, RTG-SLAM, GS-ICP SLAM, GSFusion |
| 6 | Texture par splat pour les couches statiques (A5, C3) | Une couleur par splat, flou des textures | Fort si 3 est fait (sinon faible : la cellule vaut déjà le pixel capteur) | +16 à 32 octets par splat statique; une lecture de texture filtrée par fragment | Moyen à élevé | Moyen | Gaussian Billboards, Textured Gaussians, TeSO, LGTM, Papantonakis 2025 |
| 7 | Poids de confiance par splat et échelle modulée par le contenu (A4) | Bords, valeurs aberrantes, flou des zones texturées | Faible à moyen | Négligeable | Faible | Faible | GPS-Gaussian+, SurfSplat, Keller 2013 |
| 8 | Réajustement linéaire des couleurs contre les vues d'entrée (B1) | Flou dû au recouvrement des noyaux, incohérence de couleur entre vues | Moyen sur les vues d'entrée, incertain sur les vues tenues à l'écart | 1 à 5 ms par itération pour la couche vivante sur 4 fils (estimation à mesurer) | Moyen | Moyen (résonance aux contours, amplification du bruit) | Lieber 2026 |
| 9 | Transport en espace image de la couche vivante (C4) | Budget de débit, qui bloque 3 | Indirect : 3 à 4 fois plus de splats à débit égal | Dépaquetage dans le vertex shader | Élevé | Moyen | GPS-Gaussian (cartes de paramètres), V^3 |
| 10 | Raffinement photométrique des couches statiques en tâche de fond (B2) | Tout ce qui reste sur le plateau et la maison | Potentiellement fort, incertain | Dizaines de secondes CPU pour le plateau; WebGPU pour la maison | Élevé | Élevé | RTG-SLAM, GSFusion, Gaussian-Plus-SDF SLAM, 3DGS-LM, Brush |
| 11 | Comblement des trous (A6, C5) | Zones jamais vues | Fort sur le PSNR global de la maison, cosmétique | Faible | Faible à moyen | Moyen (contenu inventé, cohérence stéréo) | Marroquim 2007 |
| 12 | Méthodes apprises pour de vraies caméras (D) | Profondeur absente ou bruitée, détails | Fort avec GPU et poids | GPU dédié | Élevé | Hors contraintes actuelles | GPS-Gaussian+, Tele360, Prompt Depth Anything, LGTM |

## 2. Diagnostic

### 2.1 Ce que montrent les images

Scène en direct (`stage_t1.png`, 25,1 dB). L'image de différence est dominée par trois motifs :
les arêtes du damier du sol (toute la surface du plateau, couche statique), le contour du danseur
et des balles (liseré blanc d'environ un pixel capteur), et les rayures du maillot. Dans la
visionneuse (`stage-view-a.png`), s'y ajoutent des pointes radiales sur les enceintes et le bord
de scène : des splats allongés produits par des observations rasantes.

Scan de maison (`house_1.png`, `house_top.png`, 21,5 dB). Dominent : les zones noires jamais
observées, les contours des meubles et des poutres, les structures fines épaissies (suspensions,
pieds). Dans la visionneuse (`house-interior.png`), un flou général, une trame régulière visible
sur les murs et les fenêtres (la grille des disques), et des franges aux arêtes des meubles.

### 2.2 Un modèle d'erreur pour hiérarchiser

Trois résultats élémentaires, pour une image de valeurs dans [0, 1] et un contour de contraste C :

- contour flouté par un noyau gaussien d'écart-type s pixels : erreur quadratique par pixel de
  longueur de contour égale à 0,234 · s · C² (intégrale de (Φ(x/s) − H(x))², soit s·(√2 − 1)/√π);
- contour déplacé de d pixels : d · C² par pixel de longueur;
- trou de fraction de surface a, d'écart C au fond : a · C².

Conséquences. (1) Un contour déplacé d'un pixel coûte environ 4 fois plus qu'un contour flouté
avec s = 1 : la justesse des silhouettes pèse plus dans le PSNR que la netteté des textures.
(2) Diviser par deux le flou ou le déplacement retire au plus 3 dB à la composante concernée.
(3) Des trous couvrant 1 % de l'image avec C² = 0,5 plafonnent à eux seuls le PSNR à 23 dB : dans
la maison, les zones non observées expliquent probablement une grande part des 21,5 dB. Le test
dispose déjà de `psnr_observed` et `coverage` (`tests/common/mod.rs`) : c'est `psnr_observed`
qu'il faut suivre pour juger la reconstruction, et `coverage` pour les trous.

Ce modèle est cohérent avec une mesure déjà consignée dans `protocol.rs` : réduire la cellule de
3,2 à 2,5 cm ajoute 265 000 splats pour environ 0,5 dB. La finesse des cellules n'est pas ce qui
limite la maison.

### 2.3 Les causes, par ordre d'importance

1. **Échantillonnage.** Anneau : 160x120 à 50 degrés, soit fy = 129 px et 2,5 cm par pixel à
   3,2 m. Maison : 192x144 à 80 degrés, soit fy = 86 px, donc 1,2 cm par pixel à 1 m, 2,3 cm à
   2 m, 3,5 cm à 3 m; la cellule de 3,2 cm vaut le pixel capteur à 2,7 m. Les vues de test
   (320x240) regardent 2 fois plus finement, la visionneuse (1280 px, ou un casque) 6 à 8 fois.
   Le flou vu par le propriétaire du projet est d'abord celui-là. Mip-Splatting le dit en toutes
   lettres : rendre à plus haute résolution que les vues d'entrée "est une tâche de
   super-résolution" et le modèle ne doit pas inventer les hautes fréquences.
2. **Composition.** Des disques gaussiens d'opacité 1 et d'écart-type 0,6 cellule couvrent bien
   (opacité cumulée supérieure à 0,9 partout) mais sous "over" trié le splat le plus proche
   impose sa couleur sur son disque : on voit les disques. Pour la scène, des splats d'opacité
   0,85 venant de 2 à 4 caméras se mélangent dans l'ordre du tri des centres : la texture résultante
   est une moyenne de phases d'échantillonnage différentes, d'où le flou et le décalage d'un pixel.
   SurfSplat décrit le même symptôme pour les méthodes feed-forward : des nuages "discrets, biaisés
   en couleur", plausibles à la résolution d'entrée, qui révèlent "de sévères artefacts en vue
   rapprochée".
3. **Redondance entre vues.** Huit nuages concaténés. La couche statique du plateau, calculée une
   seule fois et sans contrainte de temps, subit le même traitement que la couche vivante.
4. **Bords.** `min_cos = 0,2` accepte des observations à 78 degrés d'incidence, étirées jusqu'à
   `max_anisotropy = 4`, avec des normales estimées sur un voisinage de 4 pixels. Une normale
   fausse fait sortir un disque allongé de la surface : c'est la fourrure. Dans le scan, une
   cellule est créée dès la première observation, même rasante, même isolée.
5. **Rendu.** L'approximation affine de la projection (EWA) n'est exacte qu'au centre du splat;
   l'erreur croît avec la taille à l'écran (2DGS, section 4.2, d'après Zwicker 2004). Nos splats
   sont justement vus agrandis. La dilatation de 0,3 px² sans compensation d'énergie épaissit les
   structures fines vues de loin.

Point rassurant : le facteur d'empreinte retenu empiriquement (`footprint = 0,45`) est exactement
le filtre de lissage 3D de Mip-Splatting (variance 0,2, soit un écart-type de 0,447 intervalle
d'échantillonnage si on la lit en pixels au carré, lecture qui est celle de l'implémentation de
référence, citée de mémoire). La taille des splats est donc déjà au bon endroit.

Point de calibration : GSFusion, système RGB-D optimisé en ligne sur GPU, obtient 28,84 dB sur les
vues d'entraînement de ScanNet++ et 25,45 dB sur des vues nouvelles (tableau IV). Nos 21,5 et
25,1 dB sont des mesures sur vues nouvelles, avec des capteurs bien plus grossiers.

## 3. Recommandations détaillées

### (A) Changements analytiques qui tiennent dans le budget CPU et WASM actuel

#### A1. Fusion multi-vues : cohérence et meilleure vue (rang 1)

Idée. Avant d'émettre le splat d'un pixel de la caméra i, on le reprojette dans les autres
caméras. Si une autre caméra voit à travers lui, c'est un pixel volant : on le jette. Si une autre
caméra voit la même surface nettement mieux, on le jette aussi (ou on réduit son poids une fois C1
en place). Chaque point de surface est alors décrit par sa meilleure observation, au lieu d'un
empilement.

Ce que fait la littérature. FreeSplat (section 4.3) projette les gaussiennes globales dans la
nouvelle vue, apparie par pixel sous un seuil relatif de profondeur, et fusionne les paires au
lieu de concaténer, en reprochant aux méthodes antérieures une "redondance notable dans les zones
de recouvrement". GPS-Gaussian+ ne rend jamais que les deux vues sources adjacentes à la vue
cible. Le rendu de lumigraphes non structurés (Buehler 2001) pondère chaque caméra par l'écart
angulaire, la résolution et la distance au bord de son image. Holoportation combinait normales,
vote majoritaire et discontinuités de profondeur dilatées (d'après Du 2018).

Correspondance avec le code. Nouvelle fonction dans `reconstruct.rs`, appelée par
`reconstruct_ring` dans `pipeline.rs` après le calcul des `FrameGeometry` de toutes les caméras
et avant l'émission des splats. Données nouvelles par caméra : la carte de profondeur inverse
lissée (déjà calculée) et une carte de qualité.

Algorithme.

```text
qualité d'une observation :  q = cos θ · (f / z)²        (échantillons par m² de surface)
tolérance :                  τ = max(edge_threshold, 3 · sigma_bruit(z) / z)

pour chaque pixel valide (caméra i, point monde p, normale n, qualité q_i) :
    soutien = 0; violations = 0; dominé = faux
    pour chaque caméra j ≠ i :
        (u, v, z_j) = projection_j(p);  ignorer si hors image
        d_j = profondeur_j(u, v) (plus proche voisin);  ignorer si invalide
        si z_j < d_j · (1 − τ) :            # j voit plus loin à travers p
            si n · (c_j − p) > 0 : violations += 1
        sinon si |z_j − d_j| ≤ τ · d_j :     # même surface
            soutien += 1
            si q_j(u, v) > (1 + m) · q_i : dominé = vrai
        # sinon p est caché pour j : aucune information
    si violations ≥ 1 et soutien = 0 : rejeter            (pixel volant, valeur aberrante)
    sinon si dominé : rejeter (mode dur) ou poids ∝ q_i (mode normalisé, avec C1)
    sinon : émettre
```

Marge m entre 0,25 et 0,5 : dans la bande où deux caméras se valent, les deux splats restent, ce
qui évite les trous aux coutures. Départage strict par (q, identifiant de caméra) pour que deux
observations ne s'éliminent jamais mutuellement. Ajouter au poids un terme de bord (distance au
bord de l'image et à la discontinuité de profondeur la plus proche, saturée à 2 ou 3 pixels).

Coût. 9 000 splats x 7 projections par tick, moins de 0,3 ms estimé. Pour le plateau, une fois.

Effets attendus. Disparition de la plupart des splats rasants (une autre caméra voit presque
toujours mieux), des pixels volants, du dédoublement de texture; nombre de splats réduit, ce qui
finance A2. `min_cos` peut alors remonter sans créer de trous.

Pièges. Avec de vraies caméras, l'erreur d'étalonnage et les écarts d'exposition créent des
coutures : garder la bande de recouvrement et, plus tard, une correction affine de couleur par
caméra. Les surfaces vues par une seule caméra ne sont pas filtrées : c'est A4 qui les traite.

Cas particulier du plateau vide. Il est statique et calculé une fois : le passer par la même
logique (ou par `VoxelFusion` avec une cellule fine), et lui réserver B1 et B2.

#### A2. Couleur haute résolution et profondeur guidée (rang 3)

Idée. Sur tous les appareils RGB-D réels la couleur est 2 à 8 fois plus fine que la profondeur.
On place les splats à la résolution de la couleur, la profondeur étant suréchantillonnée avec la
couleur comme guide : les bords de profondeur se calent sur les bords de couleur, au sous-pixel de
profondeur près.

Ce que fait la littérature. GPS-Gaussian+ estime la profondeur à basse résolution, la remonte à
la pleine résolution de l'image, et régresse les cartes de paramètres "à pleine résolution
d'image" (1024x1024 par vue). Le pipeline de capture volumétrique de Charisoudis et al. (2025, 6 à
12 caméras Orbbec Femto Bolt) aligne la profondeur sur la couleur puis applique un filtre
bilatéral joint guidé par l'image, à plus de 60 images par seconde sur une RTX 4080 pour des
signaux 4K, avant de donner le tout à un régresseur GPS-Gaussian. Références de base : Kopf 2007
(suréchantillonnage bilatéral joint), He 2013 (filtre guidé, temps linéaire).

Correspondance avec le code. `types::RgbdFrame` : dimensions et intrinsèques distincts pour la
couleur (le plus simple : un facteur entier s). `capture::render_rgbd` : deux résolutions.
`reconstruct::FrameGeometry` : carte de profondeur inverse à la résolution couleur.

Algorithme. Travailler sur la profondeur inverse w, affine en (u, v) sur un plan (c'est déjà le
principe de `reconstruct.rs`). Pour un pixel couleur p, de position p/s dans la grille de
profondeur, avec ses 4 voisins de profondeur q (2x2) :

```text
prédiction du voisin q, par extrapolation de son plan local (gradient (a_q, b_q) déjà calculé) :
    w_q(p) = w(q) + a_q · Δu + b_q · Δv
poids :  k_q = bilin(q, p) · exp(−‖I(p) − Ī(q)‖² / (2 σ_r²)) · valide(q)
         (Ī(q) : couleur moyenne des pixels couleur couverts par q; σ_r de 10 à 20 niveaux sur 255)

si les prédictions w_q(p) valides tiennent dans la tolérance de bord :   # une seule surface
    w(p) = Σ k_q · w_q(p) / Σ k_q
sinon :                                                                 # discontinuité
    regrouper les voisins en surfaces (écart relatif > edge_threshold)
    choisir le groupe de plus grand Σ k_q, et n'interpoler que dans ce groupe
    (jamais de moyenne à travers le bord : pas de pixel volant créé)
```

La normale et les drapeaux de silhouette du pixel couleur sont ceux du groupe retenu; les tangentes
`Pu`, `Pv` sont divisées par s. Le reste de `row_to_splats` est inchangé.

Tenir le budget. À s = 2, quatre fois plus de splats. Trois moyens cumulables : A1 (redondance),
un quadtree sur le contraste comme GSFusion (une feuille par région de contraste inférieur à un
seuil, 0,1 dans l'article; chez nous : fusionner un bloc 2x2 de pixels couleur en un splat de
taille double quand il est coplanaire et de couleur presque uniforme), et C4 (transport). Pour les
couches statiques, préférer A5 (textures) à la multiplication des splats.

Coût. Reconstruction linéaire en pixels couleur : de 1,2 ms à environ 5 ms à s = 2 (estimation).
La capture simulée coûte 4 fois plus de rayons (6 ms aujourd'hui); avec de vraies caméras ce coût
n'existe pas. En simulation, ne tirer les rayons couleur fins que sur les pixels de premier plan
est une optimisation du simulateur, pas du pipeline.

Pièges. Recalage couleur-profondeur imparfait sur les appareils réels (prévoir une tolérance d'un
pixel de profondeur dans le choix du groupe). Bords de profondeur sans bord de couleur (objet de
la couleur du fond) : le poids bilinéaire seul décide, résultat équivalent à aujourd'hui.

#### A3. Fusion du scan par confiance (rang 5)

Idée. Remplacer "la première bonne observation crée, une meilleure remplace" par la fusion de
surfels classique : chaque cellule porte une confiance, les observations y entrent en moyenne
pondérée, une cellule n'est pleinement affichée qu'une fois confirmée, et une cellule contredite
est supprimée.

Ce que fait la littérature. Keller et al. (2013) : chaque point porte position, normale, rayon,
compteur de confiance et date; fusion par moyenne pondérée; les points passent d'instables à
stables quand leur confiance dépasse un seuil; les points restés instables trop longtemps et ceux
qui se trouvent devant une nouvelle mesure sont retirés (les constantes exactes, poids
exp(−γ²/2σ²) avec σ = 0,6 et seuil de stabilité 10, sont citées de mémoire). RTG-SLAM reprend
explicitement ce schéma : gaussiennes stables ou instables selon un compteur de confiance, ajout
pour trois types de pixels (nouvellement observés, forte erreur de couleur, forte erreur de
profondeur), suppression de celles qui restent instables trop longtemps. GS-ICP SLAM prend pour
forme initiale de chaque gaussienne la covariance de ses k plus proches voisins. GSFusion
n'autorise qu'une gaussienne par voxel et n'en crée que là où le contraste de l'image le demande.

Correspondance avec le code. `VoxelFusion::Accum`, `integrate`, `cell_splat`.

Algorithme.

```text
poids d'un échantillon :  α = cos θ · min(1, (pas_de_cellule / empreinte)²) · bord
mise à jour d'une cellule (somme de poids c) :
    x̄ ← (c · x̄ + α · x) / (c + α)   pour hauteur, normale, couleur;   c ← min(c + α, c_max)
    (remplacement complet conservé si l'empreinte est < refine_ratio · empreinte mémorisée)
affichage :  poids ou opacité du splat = min(1, c / c_stable)
suppression : cellule instable depuis plus de T images; ou cellule vue "à travers" :
    une image mesure, le long du rayon passant par le centre de la cellule, une profondeur
    plus lointaine de plus de 3 cellules alors que la normale de la cellule fait face à la caméra,
    et cela sur 2 images
forme : accumuler les moments d'ordre 2 des positions des échantillons dans le plan tangent
    (3 flottants : Suu, Suv, Svv); écarts-types du splat =
    clamp(κ · √λ₁,₂ , 0,2 · cellule, 0,6 · cellule), avec κ ≈ 2,1
    (une cellule uniformément couverte a √λ = 0,29 cellule, donc 0,6 cellule comme aujourd'hui;
     un fil ou un bord donnent un splat étroit dans la direction non couverte)
```

Effets attendus. Les franges aux arêtes des meubles (cellules créées par une seule observation
rasante) s'estompent puis disparaissent; les fils et pieds de meubles gardent leur largeur; les
changements de scène sont absorbés.

Pièges. Avec `c_stable` trop haut, le scan paraît se construire lentement : l'opacité progressive
règle le problème à l'affichage, et la diffusion `Append` reste valable puisque seule l'opacité
change (republier la couche comme aujourd'hui en fin de scan).

#### A4. Poids de confiance par splat, échelle modulée par le contenu (rang 7)

Ce que prédisent les réseaux au-delà de notre calcul analytique, et l'équivalent sans réseau :

| Sortie apprise | Rôle constaté dans l'article | Équivalent analytique |
|---|---|---|
| Carte d'opacité (GPS-Gaussian+, section V-E) | "élimine les valeurs aberrantes en leur prédisant une opacité extrêmement basse", surtout aux marges où la profondeur change brutalement | opacité (ou poids, avec C1) = produit de : rampe sur cos θ, distance à la discontinuité de profondeur, accord multi-vues (A1), confiance (A3) |
| Carte d'échelle (GPS-Gaussian+, section V-F) | les textures fines et la géométrie à haute fréquence "mènent à de petites gaussiennes" | multiplicateur d'empreinte dans [0,7; 1,3] décroissant avec le gradient local de couleur; SurfSplat borne son multiplicateur appris à [1/3; 3] autour de la même estimation géométrique que la nôtre |
| Borne d'opacité (SurfSplat, section 3.4) | opacité plafonnée à 0,6 pour que les splats cachés contribuent | sans objet si C1 est fait; sinon confirme notre 0,85 plutôt que 1 pour les vues superposées |
| Résidu de profondeur, raffinement itératif | cohérence entre les deux vues | lissage existant, plus moyenne des profondeurs concordantes entre caméras (sous-produit de A1) |
| Plusieurs gaussiennes par pixel (pixelSplat) | densité là où il faut | A2 (un splat par pixel couleur) |

Coût négligeable, effort faible. Piège : ne pas baisser l'opacité sous "over" trié sur une surface
vue par une seule caméra, on verrait à travers; d'où l'intérêt de faire C1 d'abord.

#### A5. Texture par splat pour les couches statiques (rang 6, partie reconstruction)

Idée. Garder un splat par cellule de 3,2 cm mais lui attacher une petite grille de couleurs
(2x2 à 4x4), remplie analytiquement par les échantillons qui tombent dans chaque case. Le détail
de couleur est découplé du nombre de primitives.

Ce que fait la littérature. Gaussian Billboards : grille N x N interpolée bilinéairement dans le
plan (u, v) du splat 2D, N = 4 recommandé (N = 8 n'apporte plus assez pour son coût), étendue de
la grille limitée au coeur du splat. Textured Gaussians : à nombre de gaussiennes égal et réduit
à 1 % du nombre par défaut, près de 2 dB de mieux que 3DGS; avec des textures RGBA 4x4, taille du modèle et temps
d'inférence environ doublés (125 vers 261 Mo, 0,073 vers 0,123 s par image dans leur
implémentation). Papantonakis et al. 2025 : taille de texel fixée en espace monde et bornée par la
fréquence d'échantillonnage des images. Toutes ces méthodes optimisent les textures. La référence
sans optimisation est TeSO (Hu et al. 2025) : des surfels bornés par les cubes d'un octree,
construits directement depuis un nuage de points, chacun avec une texture de 4x4 à 12x12 calculée
par moyenne pondérée des points voisins, un fondu exp(−d²/σ²) dans une zone souple hors du cube,
et un rayon qui s'arrête au premier surfel solide. C'est presque exactement notre grille de
cellules. LGTM (2026) montre la même idée côté feed-forward : primitives à basse résolution,
textures à haute résolution, jusqu'en 4K.

Algorithme. Dans `Accum`, remplacer la somme de couleur par N² sommes indexées par la case du
plan tangent où tombe l'échantillon (les sous-échantillons de `integrate` y tombent déjà). Poids
et règles de remplacement de A3, par case. Les cases vides héritent de la moyenne de la cellule.
Taille de texel visée : la plus petite empreinte ayant observé la cellule, bornée à cellule/4.

Dépendances. Rendu : C3. Utilité : à la résolution actuelle du capteur, seules les cellules vues
à moins de 1,4 m ont de quoi remplir 2x2 cases; le gain devient fort avec A2.

#### A6. Petits trous

Fermeture morphologique dans la grille : une colonne vide dont au moins 3 des 4 voisines de même
orientation existent, à moins d'une cellule de hauteur, reçoit un splat interpolé (poids réduit,
marqué comme interpolé pour que les métriques puissent l'exclure). Les grandes zones jamais vues
ne se comblent pas honnêtement : voir C5.

### (B) Optimisation ou raffinement en ligne

#### B1. Réajustement linéaire des couleurs (rang 8)

Idée. À géométrie fixée, l'image rendue est linéaire en fonction des couleurs des splats. On peut
donc chercher les couleurs qui reproduisent au mieux les images d'entrée par moindres carrés, sans
descente de gradient. Cela compense le flou introduit par le recouvrement des noyaux et réalise
une super-résolution multi-vues modeste quand plusieurs caméras voient la même surface.

Ce que fait la littérature. Lieber et al. 2026 ("Instant Colorization of Gaussian Splats")
résolvent par l'équation normale un problème de moindres carrés pondéré par la visibilité, pour
chaque gaussienne séparément, puis appliquent un raffinement itératif sur le résidu (image de
référence moins image rendue) pour tenir compte du mélange entre splats. Ils rapportent une
qualité élevée "en quelques itérations, ou secondes" sur RTX 4090, et précisent que la méthode
"n'est pas encore temps réel" sur des scènes complètes à pleine résolution.

Algorithme (version sans harmoniques sphériques, identique à leur algorithme 1 avec L = 0). Soit
W_{v,i}(x) la contribution du splat i au pixel x de la vue v (poids de mélange, "over" ou
normalisé) :

```text
initialisation : c_i = Σ_{v,x} W_{v,i}(x) · I_v(x) / Σ_{v,x} W_{v,i}(x)
répéter K fois (K de 2 à 5) :
    rendre Î_v pour chaque vue d'entrée;  r_v = I_v − Î_v
    c_i ← clamp(c_i + λ · Σ_{v,x} W_{v,i}(x) · r_v(x) / Σ_{v,x} W_{v,i}(x)),   λ de 0,5 à 1
```

C'est une itération de type Jacobi (SIRT en tomographie). Le rendu des vues d'entrée doit intégrer
sur l'aire du pixel source (filtre mip de C2), sinon on ajuste un modèle différent de la capture.

Correspondance avec le code. Nouveau module `refine.rs` dans `gs-core`, appuyé sur
`raster::prepare` et la boucle de composition, avec une variante qui accumule numérateur et
dénominateur par splat. Appel : une fois avec K grand pour le plateau, 1 itération par tick pour
la couche vivante si le budget le permet, 1 itération par image capturée sur les cellules visibles
pour la maison (remplace avantageusement la moyenne de couleur).

Coût (estimation à mesurer avec `bench_pipeline`). Couche vivante : 9 000 splats, 8 vues de
19 200 pixels, quelques centaines de milliers d'évaluations de noyau par itération, soit 1 à 5 ms
sur 4 fils; environ 4 fois plus en WASM sur un fil, donc à réserver alors aux couches statiques.

Pièges. Résonance aux contours et amplification du bruit du capteur si K est grand : limiter K,
borner l'écart à la couleur initiale. Géométrie fausse (silhouette décalée) : la couleur du fond
fuit dans le splat; faire A1 avant. Gain sur les vues tenues à l'écart incertain : à valider par
l'expérience 7.

#### B2. Raffinement photométrique des couches statiques en tâche de fond (rang 10)

Idée. Garder les splats liés à leur pixel ou à leur cellule et n'optimiser qu'un petit nombre de
scalaires par splat (décalage le long de la normale, deux échelles, poids, couleur ou texture)
contre les images d'entrée, quelques itérations à la fois, uniquement sur ce qui n'est pas encore
stable.

Ce que fait la littérature, et à quel prix.

| Système | Ce qui est optimisé | Vitesse mesurée | Matériel |
|---|---|---|---|
| RTG-SLAM | seulement les gaussiennes instables, seulement leurs pixels; 50 itérations par fenêtre; 5 % des pixels échantillonnés pour l'ajout | environ 16 images par seconde en direct (Azure Kinect), 17,9 sur leur scène "home" | RTX 4090, i9-13900KF |
| GSFusion | une gaussienne par voxel de 1 cm au plus, quelques itérations par image, 10 itérations globales en fin de scan (1 minute pour 340 images clés) | 6,14 images par seconde (ScanNet++), 9,73 (Replica) | RTX 3060, i7-13700 |
| Gaussian-Plus-SDF SLAM | la fusion SDF donne géométrie et couleur de base, des gaussiennes éparses ne corrigent que le résidu de couleur; optimisation toutes les 10 images, 20 itérations | plus de 150 images par seconde sur scènes réelles | RTX 4090 |
| VTGaussian-SLAM | gaussiennes liées aux pixels de profondeur : seuls couleur, rayon et opacité sont appris | non relevé | RTX 4090 |
| RGBD GS-ICP SLAM | covariances partagées avec le recalage G-ICP | jusqu'à 107 images par seconde pour le système entier | non relevé |
| 3DGS-LM | Levenberg-Marquardt avec gradient conjugué préconditionné à la place d'Adam | 20 % plus rapide que 3DGS à qualité égale | GPU, noyaux CUDA |

Lecture pour nous. (1) Le schéma qui marche en temps réel est celui de Gaussian-Plus-SDF SLAM et
de VTGaussian-SLAM : la fusion donne la géométrie, l'optimisation ne touche que l'apparence et peu
de paramètres. C'est exactement notre situation, et B1 en est la version linéaire. (2) Même ainsi,
ces vitesses sont obtenues avec des noyaux CUDA sur des GPU de bureau. Sur CPU, un passage avant
et arrière sur une image 192x144 coûte environ 3 rendus de référence : quelques itérations par
image capturée en natif, une seule en WASM (estimations). (3) Le second ordre n'est pas décisif :
3DGS-LM gagne 20 %, pas un ordre de grandeur. (4) La voie réaliste pour aller plus loin est un
rasteriseur différentiable en calcul WebGPU : Brush (Rust, wgpu, Burn) entraîne des scènes 3DGS
complètes sur ordinateur, Android et dans Chrome ou Edge, ce qui prouve la faisabilité dans notre
pile technique; c'est un chantier à part entière.

Ordre conseillé : le plateau vide d'abord (8 vues fixes, aucune contrainte de temps, quelques
dizaines de secondes CPU en tâche de fond après la capture de la plaque), puis la maison par lots
locaux pendant les temps morts du scan, avec terme de profondeur L1 contre le capteur comme dans
tous les SLAM RGB-D ci-dessus.

Ce qui est hors de portée : optimiser la couche vivante à chaque tick. 3DGStream : "dans les
12 secondes" par image sur RTX 3090 (150 puis 100 itérations); HiCoM : moins de 2 secondes par
image en traitant plusieurs images en parallèle; Instant Gaussian Stream : 2,67 secondes par image
(extrait de recherche); QUEEN : moins de 5 secondes. Deux ordres de grandeur au-dessus de 33 ms,
sur un matériel que nous n'avons pas.

### (C) Format et visionneuse

#### C1. Mélange normalisé avec pré-passe de profondeur (rang 2)

Idée. Pour des surfaces opaques décrites par des splats qui se recouvrent, la couleur d'un pixel
doit être la moyenne pondérée des splats de la surface visible, pas un empilement "over". On
détermine d'abord la surface visible (tampon de profondeur), puis on accumule couleur x poids et
poids des seuls splats proches de cette surface, puis on divise.

Ce que fait la littérature. Zwicker et al. 2001 (Surface Splatting) et Botsch et al. 2005 : passe
de visibilité, passe de mélange avec un décalage ε "vers l'observateur" qui fait se mélanger les
splats dont les profondeurs diffèrent de moins de ε, passe de normalisation qui divise la couleur
accumulée par la somme des poids. Gaussian-Plus-SDF SLAM (2025, équations 1 à 4) fait la même
chose avec des gaussiennes : test de profondeur avec un petit seuil ε contre la profondeur de la
surface, accumulation indépendante de l'ordre, moyenne pondérée, et note que le tri de 3DGS
disparaît en avant comme en rétropropagation. TeSO arrête le rayon au premier surfel solide.

Mise en oeuvre WebGL2 (`shaders.rs`, `splats.rs`).

```glsl
// Passe 1, visibilité : écriture de profondeur, pas de couleur, test LESS.
//   VS : le splat est reculé de eps le long du rayon de vue (eps = 2 à 3 écarts-types du splat,
//        borné en relatif à 1 ou 2 % de la profondeur).
//   FS : if (r2 > Rv * Rv) discard;          // Rv ≈ 2 : pas de trou, pas de halo
// Passe 2, accumulation : cible RGBA16F, blendFunc(ONE, ONE), test LEQUAL, sans écriture.
//   FS : float w = v_weight * exp(-0.5 * r2);   o_color = vec4(v_color.rgb * w, w);
// Passe 3, résolution plein écran :
//   vec4 a = texture(u_acc, uv);  if (a.a < 1e-4) discard;
//   o_color = vec4(a.rgb / a.a, 1.0) * smoothstep(0.0, tau, a.a);   // tau ≈ 0,3 : bord lissé
```

Détails. Élimination des faces arrière (normale opposée au rayon) : les deux faces d'un mur mince
ne se mélangent jamais et la moitié des splats de la maison ne sont pas dessinés. `v_weight` porte
la confiance (A3, A4), la qualité (A1) et une priorité de couche : les splats d'ombre de la couche
vivante reçoivent un poids multiplié (par exemple x16) au lieu du soulèvement `shadow_lift`. Les
poids peuvent aussi dépendre du point de vue (C6). Le tri CPU devient inutile pour ce chemin.

Référence CPU. Option `Blend::Normalized { eps }` dans `raster.rs` : première boucle pour la
profondeur minimale par pixel, seconde pour les sommes. C'est par là qu'il faut commencer : le
gain se mesure sur les vues tenues à l'écart avant d'écrire une ligne de GLSL.

Coût sur le casque. Deux passes de géométrie au lieu d'une, une cible demi-flottante et une passe
plein écran en plus; en échange, les couches cachées (plusieurs murs en enfilade dans la maison)
sont rejetées par le test de profondeur au lieu d'être mélangées, et un noyau plus étroit devient
possible (rayon de coupure 2 au lieu de 3 écarts-types, puisque la normalisation garantit
l'absence de trous). Bilan à mesurer; sur GPU mobile à tuiles, le `discard` de la passe 1 est le
point sensible.

Pièges. Rendu dans une cible RGBA16F : extension `EXT_color_buffer_half_float` ou
`EXT_color_buffer_float` à vérifier sur le navigateur du casque; sinon garder le chemin trié. Les
scènes 3DGS importées (`io.rs`), semi-transparentes par nature, gardent le chemin actuel : drapeau
par couche. Deux surfaces distinctes plus proches que ε se mélangent : ε proportionnel à la taille
du splat, pas constant.

#### C2. Splats plans exacts en perspective, filtre mip normalisé (rang 4)

Idée. Nos gaussiennes sont des disques plans. Le rendu exact d'un disque plan est l'évaluation de
la gaussienne au point où le rayon du pixel coupe son plan (2DGS, équations 8 à 10), pas la
projection affine de sa covariance. Dans un pipeline de rastérisation, il suffit de dessiner le
vrai quadrilatère 3D : l'interpolation des variables, correcte en perspective, donne (u, v) au
point d'intersection, sans calcul supplémentaire.

```glsl
// données par splat : centre p, deux vecteurs tangents Tu = s1·a1, Tv = s2·a2 (6 flottants,
// à la place des 6 termes de covariance : même empreinte dans la texture de données)
vec3 world = p + R * (corner.x * Tu + corner.y * Tv);     // R = rayon de coupure (2 ou 3)
gl_Position = u_proj * u_view * vec4(world, 1.0);
v_uv = corner * R;                                        // FS : r2 = dot(v_uv, v_uv)
```

Cas dégénérés. Un disque vu par la tranche devient une ligne. 2DGS le borne inférieurement par un
noyau fixe en espace écran, G = max(G(u(x)), G((x − c)/σ)) avec σ = √2/2 pixel, repris de Botsch
2005. Chez nous : si l'étendue projetée du petit axe est inférieure à environ 1 pixel, basculer
sur le chemin EWA actuel pour ce splat (la covariance se recalcule dans le vertex shader comme
Tu·Tuᵀ + Tv·Tvᵀ).

Filtre mip. Dans le chemin EWA conservé, remplacer la dilatation `+0,3` sans compensation par le
filtre de Mip-Splatting (équation 10) : covariance écran Σ + s·I avec s = 0,1 et opacité
multipliée par √(det Σ / det(Σ + s·I)). Trois lignes dans `SPLAT_VS` et dans `splat.rs`. Dans
l'expérience "entraînement à basse résolution, rendu jusqu'à 8 fois plus fin" de Mip-Splatting
(tableau 3, Mip-NeRF 360), la moyenne passe de 23,25 dB (3DGS) à 25,43 dB (filtre EWA normalisé)
puis 27,37 dB (méthode complète). Ces écarts viennent en grande partie de gaussiennes optimisées
trop petites, biais que nos splats dimensionnés analytiquement n'ont pas : ne pas en attendre
autant. Le bénéfice principal chez nous est pour les vues éloignées et le mode maquette.

Bénéfices. Plus d'aiguilles dues à l'approximation affine en vue rasante, géométrie exacte des
gros splats proches de l'oeil en VR, vraie profondeur par fragment (requise par C1) et vraies
coordonnées (u, v) (requises par C3).

Limite signalée par 2DGS : surfaces supposées opaques, semi-transparents mal rendus. Sans objet
pour des surfels de capteur.

#### C3. Rendu des textures par splat (rang 6, partie rendu)

Avec C1 et C2, un splat texturé coûte une lecture de texture filtrée par fragment : les grilles
N x N de tous les splats statiques sont rangées dans un atlas RGBA8 (ou RGB565) avec filtrage
bilinéaire matériel; le vertex shader calcule l'origine du bloc à partir de l'indice du splat, le
fragment shader lit à `origine + (v_uv / étendue + 0,5) · N` texels, coordonnées bornées au bloc.

Noyau. Avec un noyau gaussien large (0,6 cellule), plus de la moitié de la couleur au centre d'une
cellule vient des voisines, dont la texture y est extrapolée : la texture serait floutée. Il faut
soit un noyau étroit sous mélange normalisé (écart-type de 0,35 cellule environ : le mélange
devient une partition douce, chaque cellule domine chez elle), soit le fondu de TeSO (poids 1 dans
la cellule, exp(−d²/σ²) au-delà de sa frontière). Les deux exigent C1.

Format. Message `Layer` étendu par un drapeau, ou message d'atlas séparé (`layer_id`, premier
indice, N, texels). À N = 4 en RGB565 : 32 octets par splat statique en plus des 16 actuels, soit
environ 20 Mo pour 416 000 splats, envoyés une fois; les cellules sans détail peuvent rester à
N = 1 (drapeau par splat), ce qui est l'idée de l'allocation adaptative de Papantonakis 2025.

#### C4. Transport en espace image pour la couche vivante (rang 9)

Constat. Une gaussienne alignée sur un pixel est entièrement déterminée par la carte de
profondeur, l'étalonnage et l'image couleur. GPS-Gaussian définit d'ailleurs ses gaussiennes comme
des "cartes de paramètres" sur les plans image. Les transmettre comme 16 octets indépendants par
splat ignore cette structure. V^3 (2024) va au bout de l'idée pour le mobile : attributs rangés en
grilles 2D et compressés par les codecs vidéo matériels.

Proposition. Par caméra et par tick : profondeur inverse sur 16 bits et masque de premier plan,
normale encodée sur 2 octets, 1 octet de drapeaux et de poids, plus l'image couleur (JPEG décodé
par le navigateur, ou vidéo plus tard). Le vertex shader lit ces cartes par `texelFetch`,
déprojette et construit Tu, Tv. Environ 5 octets par pixel de premier plan plus la couleur
compressée, contre 16 : 3 à 4 fois plus de splats à débit égal. À 30 Hz, 9 000 splats font
aujourd'hui 4,3 Mo/s; 36 000 (A2 à s = 2) feraient 17 Mo/s dans le format actuel.

Effort élevé (codec, visionneuse, tests de bout en bout); à n'engager que si A2 est retenu et que
A1 plus le quadtree ne suffisent pas à tenir le débit.

#### C5. Trous visibles (rang 11)

Remplissage en espace écran par pyramide "pull-push" (Marroquim et al. 2007, plus de 50 millions
de points par seconde sur le GPU de l'époque, reconstruction comprise) : chaque pixel non couvert
reçoit la moyenne floue des pixels couverts voisins. Simple après C1 (la passe de résolution sait
quels pixels sont vides). Limites : le contenu est inventé, il peut différer entre les deux yeux
et scintiller; à restreindre aux trous de quelques pixels, les grandes zones non vues restant
signalées comme telles (teinte neutre plutôt que noir, qui pénalise autant le PSNR que l'oeil).

#### C6. Pondération dépendante du point de vue (après C1)

Sous mélange normalisé, multiplier le poids d'un splat de la couche vivante par
max(0, cos de l'angle entre le rayon de vue et le rayon de sa caméra source)^p réalise le rendu
de lumigraphe non structuré avec des splats : la caméra la plus proche de la direction de vue
domine. Coût en format nul : les splats sont émis groupés par caméra, il suffit de transmettre le
nombre de splats par caméra dans l'en-tête de couche et les centres des caméras (déjà dans
`SessionState`). Gain nul dans la simulation (ombrage diffus); utile avec de vraies caméras
(reflets, écarts d'exposition). C'est l'alternative sans apprentissage aux harmoniques
sphériques.

### (D) Méthodes apprises pour la feuille de route avec de vraies caméras

Aucune n'est utilisable aujourd'hui (pas de poids, pas de GPU), mais elles fixent la cible.

- **Gaussiennes par pixel pour l'humain en direct.** GPS-Gaussian+ (Zhou et al. 2024) : deux vues
  1024x1024, 30 ms de traitement des vues sources puis 1,9 ms par vue rendue sur RTX 3090 avec
  TensorRT, environ 25 images par seconde de bout en bout; entraîné sans supervision de
  profondeur (perte de rendu et régularisation géométrique).
  Limite déclarée : artefacts fantômes sur les zones peu texturées, étalonnage précis et
  synchronisation stricte requis. Tele360 (Tu et al., septembre 2026) : 4 à 6 caméras RGB non
  étalonnées, entrée 2K, plus de 25 images par seconde sur un GPU grand public.
- **Avec capteurs RGB-D.** Charisoudis et al. 2025 : chaîne complète RGB-D vers gaussiennes
  (GPS-Gaussian affiné), aperçu en direct à 5-10 images par seconde. Prompt Depth Anything (Lin et
  al. 2024) : profondeur métrique jusqu'en 4K à partir d'un LiDAR basse résolution et de l'image,
  c'est-à-dire la version apprise de A2.
- **Fusion et textures apprises.** FreeSplat (fusion des gaussiennes redondantes), SurfSplat
  (a priori de continuité de surface, métrique HRRC), LGTM (textures par primitive en
  feed-forward, 4K), ReCoSplat (mise à jour causale d'une scène persistante), DepthSplat et
  MVSplat (profondeur multi-vues).
- **Scan.** Gaussian-Plus-SDF SLAM et VTGaussian-SLAM pour l'architecture; un port WebGPU de leur
  boucle d'optimisation serait la suite naturelle de B2.

Les recommandations A à C ne sont pas perdues à ce moment-là : fusion des vues, mélange normalisé,
textures et transport en espace image servent à l'identique derrière un réseau.

## 4. Ce qui ne vaut pas la peine pour nous

| Technique | Pourquoi l'écarter |
|---|---|
| Optimisation par image de la couche vivante (3DGStream, HiCoM, QUEEN, Instant Gaussian Stream) | 2 à 12 secondes par image sur GPU de bureau (RTX 3090 pour 3DGStream); nous avons 33 ms sur CPU |
| Densification et élagage (contrôle adaptatif de 3DGS, MCMC, Mini-Splatting, Taming 3DGS) | Ce sont des stratégies à l'intérieur d'une boucle d'optimisation que nous n'avons pas; la profondeur métrique nous donne d'emblée une initialisation dense, et EDGS montre qu'une initialisation dense rend la densification inutile. Seule idée transférable : élaguer les splats dont le poids de mélange cumulé sur les vues d'entrée est négligeable (critère de LightGaussian et Mini-Splatting), calculable avec notre rasteriseur |
| Harmoniques sphériques | 9 à 45 octets de plus par splat; avec 2 ou 3 caméras par point de surface l'ajustement est mal posé; la simulation est diffuse. C6 fait mieux pour moins cher. Si un jour il en faut, le solveur de Lieber 2026 les ajuste sans descente de gradient |
| StopThePop, tri par pixel | Rasteriseur logiciel en mode calcul (CUDA); le tri par pixel complet est 100 fois plus lent selon l'article, leur version hiérarchique suppose ce rasteriseur. Non portable en WebGL2. C1 supprime le popping par construction |
| Rendu sans tri par somme pondérée (Hou et al. 2024), Mobile-GS | Exigent un modèle réentraîné (opacité dépendante de la vue ou poids appris); Mobile-GS signale lui-même des artefacts de transparence là où la géométrie se superpose; le gain mesuré par Hou et al. est de 1,23 fois sur Snapdragon 8 Gen 3. C1 obtient l'indépendance à l'ordre sans apprentissage parce que nos surfaces sont opaques |
| Transparence stochastique (StochasticSplats, Gaussian Stippling) | Bruit à faible nombre d'échantillons, à débruiter dans le temps ou par réseau : mauvais candidat en VR |
| Textures alpha ou RGBA apprises (Textured Gaussians, BBSplat) | Les formes arbitraires par carte alpha n'existent que par optimisation; nous n'en retenons que la grille de couleur remplie analytiquement |
| Splatting de triangles (Triangle Splatting+) | Optimisation complète requise; change la nature du produit |
| TSDF et extraction de maillage comme représentation finale | Un champ de distance tronqué lisse la géométrie et représente mal ce qui est plus fin que quelques voxels (limite classique, non remesurée ici); il faudrait de plus un second mode de rendu. Notre grille de surfels orientés par face fait le même travail de fusion |
| Compression entropique des splats (grilles auto-organisées, codecs appris) pour le flux vivant | Coût CPU par tick; C4 gagne davantage en exploitant la structure image |
| Second ordre (3DGS-LM) comme accélérateur | 20 % de gain mesuré sur GPU : ne change pas la faisabilité sur CPU |
| Viewers WebGPU (WebSplatter : 1,2 à 4,5 fois plus rapide que les visionneuses web existantes) | À surveiller; la disponibilité de WebGPU en session WebXR sur le casque visé reste à vérifier; ne règle aucun défaut de fidélité |

## 5. Plan d'expérimentation

Outils existants : rasteriseur CPU de référence (`raster.rs`), vues tenues à l'écart 320x240
(`tests/novel_view.rs`), `psnr`, `psnr_observed`, `coverage`. Chaque étape se juge d'abord sur
CPU, avant tout travail de shader.

0. **Instrumentation** (préalable). (a) Rendre aussi les vues de test en 640x480 et 1280x960 :
   c'est la métrique HRRC de SurfSplat, en mieux puisque nous avons la vraie référence à toute
   résolution. (b) Décomposer l'erreur par masque de la référence : bande de 2 pixels autour des
   discontinuités de profondeur (silhouettes), bande autour des contours de couleur (texture),
   intérieur, non observé. (c) Ajouter SSIM. (d) Deux vues de test rapprochées (0,5 m) et deux
   rasantes. Critère : savoir, avant de coder, quelle composante pèse combien.
1. **Oracles sans code nouveau.** Capteurs à résolution x2 et x4 (plafond de A2); une seule
   caméra contre huit sur une vue où une seule suffit (coût de la superposition); normales exactes
   injectées depuis le simulateur dans un test (part de la fourrure due aux normales). Métrique :
   PSNR par composante.
2. **A1, fusion multi-vues.** Attendu : hausse du PSNR dans la bande de silhouette de la scène,
   nombre de splats dynamiques en baisse de 40 % ou plus, `coverage` inchangée (pas de trou).
   Refaire le balayage de `footprint` et `min_cos` après coup : l'optimum va bouger.
3. **C1 dans le rasteriseur CPU.** Attendu : hausse de `psnr_observed` pour la maison et la scène,
   et baisse de l'énergie haute fréquence parasite sur les aplats (variance du laplacien de
   l'erreur sur le masque "intérieur"), qui quantifie l'aspect pointillé. Balayer le rayon de
   coupure et l'écart-type du noyau (0,35 à 0,6 cellule).
4. **C2.** Attendu : gain sur les vues rapprochées et rasantes de l'étape 0, neutralité ailleurs;
   filtre mip : gain sur une vue éloignée (mode maquette), structures fines moins épaisses.
5. **C1 et C2 en WebGL2.** Égalité pixel à pixel avec la référence CPU (test existant du harnais),
   puis temps d'image sur le casque, 72 Hz tenus avec 400 000 splats; mesurer avec et sans
   pré-passe, avec rayon de coupure 2 et 3.
6. **A3 et A4.** Attendu : PSNR de la bande de silhouette de la maison, intersection sur union du
   masque de couverture contre le masque réel pour les objets fins, nombre de cellules supprimées
   par creusement.
7. **B1.** Contrôle : PSNR sur les vues d'entrée en hausse franche. Critère de décision : PSNR sur
   vues tenues à l'écart non dégradé à K = 1, 2, 5; temps par itération.
8. **A2.** PSNR à 320x240 et 640x480, erreur de silhouette, nombre de splats et octets par tick
   avec et sans quadtree; comparer au plafond mesuré à l'étape 1.
9. **A5 et C3** (si 8 est concluant). PSNR à 640x480 et 1280x960 de la maison à nombre de splats
   constant, octets, temps d'image sur le casque.
10. **B2 sur le plateau.** Courbe PSNR tenu à l'écart en fonction du temps de calcul; arrêt si
    moins de 0,5 dB après 30 secondes.
11. **C5.** Écart entre `psnr` et `psnr_observed` de la maison, et essai de confort en stéréo.

Règle de décision proposée : garder un changement s'il apporte au moins 0,3 dB sur la composante
qu'il vise sans en dégrader une autre de plus de 0,1 dB, ou une amélioration visible à 1280 px
sans coût mesurable sur le casque.

## 6. Références

Légende : [L] sections méthode, temps et limites lues dans le texte intégral; [R] résumé arXiv
seulement; [C] classique hors arXiv, cité d'après extraits de recherche ou bibliographies lues.

Rendu et filtrage
- [L] Z. Yu et al., "Mip-Splatting: Alias-free 3D Gaussian Splatting", 2023. arXiv:2311.16493. https://arxiv.org/abs/2311.16493
- [L] B. Huang et al., "2D Gaussian Splatting for Geometrically Accurate Radiance Fields", 2024. arXiv:2403.17888. https://arxiv.org/abs/2403.17888
- [R] M. Younes, A. Boukhayma, "Anti-Aliased 2D Gaussian Splatting", 2025. arXiv:2506.11252. https://arxiv.org/abs/2506.11252
- [R] X. Song et al., "SA-GS: Scale-Adaptive Gaussian Splatting for Training-Free Anti-Aliasing", 2024. arXiv:2403.19615. https://arxiv.org/abs/2403.19615
- [R] L. Huang et al., "On the Error Analysis of 3D Gaussian Splatting and an Optimal Projection Strategy", 2024. arXiv:2402.00752. https://arxiv.org/abs/2402.00752
- [R] P. Dai et al., "High-quality Surface Reconstruction using Gaussian Surfels", 2024. arXiv:2404.17774. https://arxiv.org/abs/2404.17774
- [R] B. Kerbl et al., "3D Gaussian Splatting for Real-Time Radiance Field Rendering", 2023. arXiv:2308.04079. https://arxiv.org/abs/2308.04079
- [C] M. Zwicker, H. Pfister, J. van Baar, M. Gross, "Surface Splatting", SIGGRAPH 2001, p. 371-378.
- [C] M. Zwicker, J. Räsänen, M. Botsch, C. Dachsbacher, M. Pauly, "Perspective Accurate Splatting", Graphics Interface 2004, p. 247-254.
- [C] M. Botsch, A. Hornung, M. Zwicker, L. Kobbelt, "High-Quality Surface Splatting on Today's GPUs", Symposium on Point-Based Graphics 2005. https://www.cs.umd.edu/~zwicker/publications/HighQualitySurfaceSplatting-PBG05.pdf
- [C] H. Pfister, M. Zwicker, J. van Baar, M. Gross, "Surfels: Surface Elements as Rendering Primitives", SIGGRAPH 2000, p. 335-342.
- [C] R. Marroquim, M. Kraus, P. R. Cavalcanti, "Efficient Point-Based Rendering Using Image Reconstruction", Symposium on Point-Based Graphics 2007, p. 101-108.

Ordre, popping, mobile
- [L, partiel] L. Radl et al., "StopThePop: Sorted Gaussian Splatting for View-Consistent Real-time Rendering", 2024. arXiv:2402.00525. https://arxiv.org/abs/2402.00525
- [R] F. Hahlbohm et al., "Efficient Perspective-Correct 3D Gaussian Splatting Using Hybrid Transparency", 2024. arXiv:2410.08129. https://arxiv.org/abs/2410.08129
- [L, partiel] Q. Hou et al., "Sort-free Gaussian Splatting via Weighted Sum Rendering", 2024. arXiv:2410.18931. https://arxiv.org/abs/2410.18931
- [L, partiel] X. Du et al., "Mobile-GS: Real-time Gaussian Splatting for Mobile Devices", 2026. arXiv:2603.11531. https://arxiv.org/abs/2603.11531
- [L, partiel] S. Kheradmand et al., "StochasticSplats: Stochastic Rasterization for Sorting-Free 3D Gaussian Splatting", 2025. arXiv:2503.24366. https://arxiv.org/abs/2503.24366
- [R] Z. Huang et al., "Gaussian Stippling: Efficient Sorting-Free 3D Gaussian Rendering through Hybrid Sampling and Spatiotemporal Reconstruction", 2026. arXiv:2609.38488. https://arxiv.org/abs/2609.38488
- [L, partiel] X. Tu et al., "VRSplat: Fast and Robust Gaussian Splatting for Virtual Reality", 2025. arXiv:2505.10144. https://arxiv.org/abs/2505.10144 (plus de 72 images par seconde, mais rendu sur RTX 4090 et affiché sur Quest 3)
- [R] Y. Han et al., "WebSplatter: Enabling Cross-Device Efficient Gaussian Splatting in Web Browsers via WebGPU", 2026. arXiv:2602.03207. https://arxiv.org/abs/2602.03207

Gaussiennes alignées sur les pixels, capture humaine en direct
- [L] B. Zhou et al., "GPS-Gaussian+: Generalizable Pixel-wise 3D Gaussian Splatting for Real-Time Human-Scene Rendering from Sparse Views", 2024. arXiv:2411.11363. https://arxiv.org/abs/2411.11363
- [R] S. Zheng et al., "GPS-Gaussian: Generalizable Pixel-wise 3D Gaussian Splatting for Real-time Human Novel View Synthesis", 2023. arXiv:2312.02155. https://arxiv.org/abs/2312.02155
- [L] B. He et al., "SurfSplat: Conquering Feedforward 2D Gaussian Splatting with Surface Continuity Priors", 2026. arXiv:2602.02000. https://arxiv.org/abs/2602.02000
- [L, partiel] Y. Wang et al., "FreeSplat: Generalizable 3D Gaussian Splatting Towards Free-View Synthesis of Indoor Scenes", 2024. arXiv:2405.17958. https://arxiv.org/abs/2405.17958
- [R] S. Zhang et al., "Gaussian Graph Network: Learning Efficient and Generalizable Gaussian Representations from Multi-view Images", 2025. arXiv:2503.16338. https://arxiv.org/abs/2503.16338
- [R] D. Charatan et al., "pixelSplat: 3D Gaussian Splats from Image Pairs for Scalable Generalizable 3D Reconstruction", 2023. arXiv:2312.12337. https://arxiv.org/abs/2312.12337
- [R] Y. Chen et al., "MVSplat: Efficient 3D Gaussian Splatting from Sparse Multi-View Images", 2024. arXiv:2403.14627. https://arxiv.org/abs/2403.14627
- [R] H. Xu et al., "DepthSplat: Connecting Gaussian Splatting and Depth", 2024. arXiv:2410.13862. https://arxiv.org/abs/2410.13862
- [L, partiel] H. Tu et al., "Tele360: Real-Time Feed-Forward Human Reconstruction from Sparse Unposed Cameras", 2026. arXiv:2609.15032. https://arxiv.org/abs/2609.15032
- [R] H. Tu et al., "Tele-Aloha: A Low-budget and High-authenticity Telepresence System Using Sparse RGB Cameras", 2024. arXiv:2405.14866. https://arxiv.org/abs/2405.14866
- [L, partiel] A. Charisoudis et al., "A Fast Volumetric Capture and Reconstruction Pipeline for Dynamic Point Clouds and Gaussian Splats", 2025. arXiv:2512.15719. https://arxiv.org/abs/2512.15719
- [R] Y. Hu et al., "Low Latency Point Cloud Rendering with Learned Splatting", 2024. arXiv:2409.16504. https://arxiv.org/abs/2409.16504
- [R] H. Lin et al., "Prompting Depth Anything for 4K Resolution Accurate Metric Depth Estimation", 2024. arXiv:2412.14015. https://arxiv.org/abs/2412.14015
- [R] F. Cheng et al., "ReCoSplat: Online Feed-Forward Gaussian Splatting via Render-and-Compare", 2026. arXiv:2603.09968. https://arxiv.org/abs/2603.09968

Détail par primitive
- [L] B. Chao et al., "Textured Gaussians for Enhanced 3D Scene Appearance Modeling", 2024. arXiv:2411.18625. https://arxiv.org/abs/2411.18625
- [L] S. Weiss, D. Bradley, "Gaussian Billboards: Expressive 2D Gaussian Splatting with Textures", 2024. arXiv:2412.12734. https://arxiv.org/abs/2412.12734
- [L] Y. Hu et al., "TeSO: Representing and Compressing 3D Point Cloud Scenes with Textured Surfel Octree", 2025. arXiv:2508.07083. https://arxiv.org/abs/2508.07083
- [L, partiel] P. Papantonakis et al., "Content-Aware Texturing for Gaussian Splatting", 2025. arXiv:2512.02621. https://arxiv.org/abs/2512.02621
- [L, partiel] Y. Lao et al., "Less Gaussians, Texture More: 4K Feed-Forward Textured Splatting", 2026. arXiv:2603.25745. https://arxiv.org/abs/2603.25745
- [R] D. Svitov et al., "BillBoard Splatting (BBSplat): Learnable Textured Primitives for Novel View Synthesis", 2024. arXiv:2411.08508. https://arxiv.org/abs/2411.08508
- [R] V. Rong et al., "GStex: Per-Primitive Texturing of 2D Gaussian Splatting for Decoupled Appearance and Geometry Modeling", 2024. arXiv:2409.12954. https://arxiv.org/abs/2409.12954
- [R] H. C. Nguyen et al., "IBGS: Image-Based Gaussian Splatting", 2025. arXiv:2511.14357. https://arxiv.org/abs/2511.14357
- [R] N. Kelkar et al., "Bake It Till You Make It: Ultrafast Spatial Texture-Atlas Splatting", 2026. arXiv:2607.13808. https://arxiv.org/abs/2607.13808

SLAM RGB-D et fusion
- [L] Z. Peng et al., "RTG-SLAM: Real-time 3D Reconstruction at Scale using Gaussian Splatting", 2024. arXiv:2404.19706. https://arxiv.org/abs/2404.19706
- [L] J. Wei, S. Leutenegger, "GSFusion: Online RGB-D Mapping Where Gaussian Splatting Meets TSDF Fusion", 2024. arXiv:2408.12677. https://arxiv.org/abs/2408.12677
- [L] Z. Peng et al., "Gaussian-Plus-SDF SLAM: High-fidelity 3D Reconstruction at 150+ fps", 2025. arXiv:2509.11574. https://arxiv.org/abs/2509.11574
- [L, partiel] P. Hu, Z. Han, "VTGaussian-SLAM: RGBD SLAM for Large Scale Scenes with Splatting View-Tied 3D Gaussians", 2025. arXiv:2506.02741. https://arxiv.org/abs/2506.02741
- [L, partiel] S. Ha et al., "RGBD GS-ICP SLAM", 2024. arXiv:2403.12550. https://arxiv.org/abs/2403.12550
- [R] N. Keetha et al., "SplaTAM: Splat, Track & Map 3D Gaussians for Dense RGB-D SLAM", 2023. arXiv:2312.02126. https://arxiv.org/abs/2312.02126
- [R] H. Matsuki et al., "Gaussian Splatting SLAM", 2023. arXiv:2312.06741. https://arxiv.org/abs/2312.06741
- [R] H. Huang et al., "Photo-SLAM: Real-time Simultaneous Localization and Photorealistic Mapping for Monocular, Stereo, and RGB-D Cameras", 2023. arXiv:2311.16728. https://arxiv.org/abs/2311.16728
- [R] L. Li et al., "RTGS: Real-Time 3D Gaussian Splatting SLAM via Multi-Level Redundancy Reduction", 2025. arXiv:2510.06644. https://arxiv.org/abs/2510.06644
- [R] J. Bao et al., "LightSplat: Real-Time High-Fidelity 3D Gaussian SLAM with Loop Closure", 2026. arXiv:2609.07274. https://arxiv.org/abs/2609.07274 (8 images par seconde en moyenne)
- [C] M. Keller, D. Lefloch, M. Lambers, S. Izadi, T. Weyrich, A. Kolb, "Real-time 3D Reconstruction in Dynamic Scenes using Point-based Fusion", 3DV 2013. https://reality.tf.fau.de/projects/kinect/keller13realtime.pdf
- [C] R. Newcombe et al., "KinectFusion: Real-time dense surface mapping and tracking", ISMAR 2011, p. 127-136.
- [C] B. Curless, M. Levoy, "A volumetric method for building complex models from range images", SIGGRAPH 1996, p. 303-312.

Optimisation rapide et flux
- [L] D. Lieber, A. Mock, N. Wandel, "Instant Colorization of Gaussian Splats", 2026. arXiv:2604.17155. https://arxiv.org/abs/2604.17155
- [L, partiel] L. Höllein et al., "3DGS-LM: Faster Gaussian-Splatting Optimization with Levenberg-Marquardt", 2024. arXiv:2409.12892. https://arxiv.org/abs/2409.12892
- [L, partiel] J. Sun et al., "3DGStream: On-the-Fly Training of 3D Gaussians for Efficient Streaming of Photo-Realistic Free-Viewpoint Videos", 2024. arXiv:2403.01444. https://arxiv.org/abs/2403.01444
- [R] Q. Gao et al., "HiCoM: Hierarchical Coherent Motion for Streamable Dynamic Scene with 3D Gaussian Splatting", 2024. arXiv:2411.07541. https://arxiv.org/abs/2411.07541
- [R] S. Girish et al., "QUEEN: QUantized Efficient ENcoding of Dynamic Gaussians for Streaming Free-viewpoint Videos", 2024. arXiv:2412.04469. https://arxiv.org/abs/2412.04469
- [R] J. Yan et al., "Instant Gaussian Stream: Fast and Generalizable Streaming of Dynamic Scene Reconstruction via Gaussian Splatting", 2025. arXiv:2503.16979. https://arxiv.org/abs/2503.16979
- [R] D. Kotovenko et al., "EDGS: Eliminating Densification for Efficient Convergence of 3DGS", 2025. arXiv:2504.13204. https://arxiv.org/abs/2504.13204
- [R] F. Hahlbohm et al., "Faster-GS: Analyzing and Improving Gaussian Splatting Optimization", 2026. arXiv:2602.09999. https://arxiv.org/abs/2602.09999
- [R] A. Meuleman et al., "On-the-fly Reconstruction for Large-Scale Novel View Synthesis from Unposed Images", 2025. arXiv:2506.05558. https://arxiv.org/abs/2506.05558
- Logiciel : Brush, moteur de reconstruction 3DGS en Rust sur wgpu et Burn, A. Brussee. https://github.com/ArthurBrussee/brush (d'après la description du dépôt; non testé)

Compaction, compression, transport
- [R] G. Fang, B. Wang, "Mini-Splatting: Representing Scenes with a Constrained Number of Gaussians", 2024. arXiv:2403.14166. https://arxiv.org/abs/2403.14166
- [R] S. S. Mallick et al., "Taming 3DGS: High-Quality Radiance Fields with Limited Resources", 2024. arXiv:2406.15643. https://arxiv.org/abs/2406.15643
- [R] S. Kheradmand et al., "3D Gaussian Splatting as Markov Chain Monte Carlo", 2024. arXiv:2404.09591. https://arxiv.org/abs/2404.09591
- [R] Z. Fan et al., "LightGaussian: Unbounded 3D Gaussian Compression with 15x Reduction and 200+ FPS", 2023. arXiv:2311.17245. https://arxiv.org/abs/2311.17245
- [R] P. Wang et al., "V^3: Viewing Volumetric Videos on Mobiles via Streamable 2D Dynamic Gaussians", 2024. arXiv:2409.13648. https://arxiv.org/abs/2409.13648
- [R] J. Held et al., "Triangle Splatting+: Differentiable Rendering with Opaque Triangles", 2025. arXiv:2509.25122. https://arxiv.org/abs/2509.25122

Mélange de vues et prétraitement de profondeur
- [C] C. Buehler, M. Bosse, L. McMillan, S. Gortler, M. Cohen, "Unstructured Lumigraph Rendering", SIGGRAPH 2001. https://www.eecs.harvard.edu/~sjg/papers/ulr.pdf
- [C] R. Du et al., "Montage4D: Interactive Seamless Fusion of Multiview Video Textures", I3D 2018 (décrit aussi les poids de Holoportation, Orts-Escolano et al. 2016). https://hhoppe.com/proj/montage4d/
- [C] J. Kopf, M. Cohen, D. Lischinski, M. Uyttendaele, "Joint Bilateral Upsampling", ACM Transactions on Graphics 26(3), SIGGRAPH 2007.
- [C] K. He, J. Sun, X. Tang, "Guided Image Filtering", ECCV 2010 et IEEE TPAMI 35(6), 2013, p. 1397-1409.
- [R] J. T. Barron, B. Poole, "The Fast Bilateral Solver", 2015. arXiv:1511.03296. https://arxiv.org/abs/1511.03296

### Points non vérifiés ou incertains

- Constantes de Keller 2013 (σ = 0,6, seuil de stabilité 10) et formules de pénalité de Buehler
  2001 : citées de mémoire, non relues.
- Temps de 2,67 s par image d'Instant Gaussian Stream : extrait de recherche, non relu dans le
  tableau de l'article. Matériel de HiCoM, QUEEN et RGBD GS-ICP SLAM : non relevé.
- Budget de splats d'un Quest 3 autonome : aucune source évaluée par les pairs trouvée; la seule
  indication (environ 200 000 splats visibles conseillés) vient de la documentation d'une boîte à
  outils Unity et n'est pas transposable telle quelle à WebGL2.
- Disponibilité des cibles de rendu demi-flottantes et coût réel de la pré-passe sur le navigateur
  du casque : à mesurer (expérience 5).
- Tous les coûts "pour nous" et tous les gains en dB pour Splat Live : estimations.
