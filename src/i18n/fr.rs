// SPDX-License-Identifier: MIT OR Apache-2.0

//! French.

use super::*;

pub static FR: Strings = Strings {
    settings: "Réglages",
    settings_tip: if SHOW_AUTOTONE_CENTERING && RAW_SETTING {
        "Thème, langue, courbe de tons RAW et Ajustement auto (Cmd+,)"
    } else if SHOW_AUTOTONE_CENTERING {
        "Thème, langue et Ajustement auto (Cmd+,)"
    } else if RAW_SETTING {
        "Thème, langue et courbe de tons RAW (Cmd+,)"
    } else {
        "Thème et langue (Cmd+,)"
    },
    settings_title: "Réglages",
    settings_theme: "Thème",
    settings_language: "Langue",
    settings_auto_tone: "Ajustement auto",
    settings_raw: "Fichiers RAW",
    apple_tone_curve: "Courbe de tons Apple",
    apple_tone_curve_hint: "Activée, les fichiers RAW ont le même rendu que dans Photos et Preview. Désactivée, LightPhotos applique sa propre courbe, comme dans le navigateur.",
    autotone_center_range: "Centrer la plage",
    autotone_center_range_tip: "Place le milieu de la plage de l'histogramme à 50 %. La forme de la photo reste en place.",
    autotone_center_median: "Centrer le pixel médian",
    autotone_center_median_tip: "Place à 50 % le pixel plus clair que la moitié de la photo. Une photo surtout sombre devient plus claire.",
    form_general: "Général",
    theme_dark: "Sombre",
    theme_medium: "Moyen",
    theme_light: "Clair",

    open_folder: "Ouvrir un dossier",
    open_folder_tip: "Ajouter un dossier de photos à Dossiers et l'ouvrir (Cmd+O)",
    back_to_grid_tip: "Retour à la grille (G)",
    fullscreen_tip: "Plein écran (F)",
    slideshow_menu_tip: "Options du diaporama",
    slideshow_play: "Lancer le diaporama (Cmd+Enter)",
    slideshow_duration: "Durée d'affichage",
    slideshow_random: "Aléatoire",
    slideshow_fill: "Zoom pour remplir",
    seconds_suffix: " s",
    shown_thumb_tip: "Affichage de la vignette",
    shown_embedded_tip: "Affichage du JPEG intégré au fichier",
    shown_preview_tip: "Affichage d'un décodage d'aperçu, sous la pleine résolution",
    shown_full_tip: "Affichage en pleine résolution",
    help_tip: "Raccourcis clavier (?)",
    metadata_title: "Métadonnées",
    info_tab_tip: "Métadonnées de la photo (I)",
    info_no_selection: "Aucune photo sélectionnée",
    info_file: "Fichier",
    info_camera: "Appareil",
    info_exposure: "Exposition",
    info_date: "Date",
    info_location: "Lieu",
    info_name: "Nom",
    info_dimensions: "Dimensions",
    info_modified: "Modifié",
    info_make: "Marque",
    info_model: "Modèle",
    info_lens: "Objectif",
    info_shutter: "Vitesse",
    info_aperture: "Ouverture",
    info_iso: "ISO",
    info_focal_length: "Focale",
    info_exposure_comp: "Comp. d'expo.",
    info_flash: "Flash",
    info_white_balance: "Balance des blancs",
    info_captured: "Prise de vue",
    info_latitude: "Latitude",
    info_longitude: "Longitude",
    info_altitude: "Altitude",
    info_open_in_maps: "Ouvrir dans Plans",
    flash_fired: "Déclenché",
    flash_did_not_fire: "Non déclenché",
    wb_auto: "Auto",
    wb_manual: "Manuelle",
    landing_prompt: "Choisissez un dossier de photos pour commencer",
    landing_tagline: "Triez, notez et développez vos photos RAW, rapidement.",
    landing_allow_note: if WEB {
        "Quand le navigateur demande « Autoriser ce site à modifier des fichiers ? \u{2026} », cliquez sur Autoriser.\n\
         Cela permet à l'application d'enregistrer les notes et les retouches dans un sous-dossier .lightphotos.\n\
         Vos photos originales ne sont jamais modifiées."
    } else {
        ""
    },
    opening: "Ouverture\u{2026}",
    picker_title: "Choisir un dossier de photos",
    add_folder: "Ajouter un dossier",
    add_folder_tip: "Ajouter un dossier de photos (Cmd+O)",
    remove_folder: "Retirer le dossier de LightPhotos",
    folder_missing: |folder| format!("{folder} est introuvable. Reconnectez son disque, ou faites un clic droit pour le retirer."),
    folder_not_kept: |folder| format!("Ce navigateur n'a plus accès à {folder}. Ajoutez-le de nouveau, ou faites un clic droit pour le retirer."),
    select_photos_hint: "Sélectionnez des photos pour les noter, les mettre en pile et les exporter ici.",
    tour: "Visite",
    tour_tip: "Une visite rapide pour tout situer",
    take_tour: "Faire la visite",
    skip_tour: "Passer la visite",
    tour_back: "Retour",
    tour_next: "Suivant",
    tour_done: "Terminé",
    tour_step_of: |n, m| format!("{n} sur {m}"),
    tour_loupe_icons: [
        "Recadrez, faites pivoter et redressez la photo. Appuyez sur R pour l'ouvrir.",
        "Passez le pinceau sur les taches et la poussière pour les retirer. Appuyez sur Q pour l'ouvrir.",
        "Placez côte à côte les prises d'une pile pour choisir la meilleure.",
    ],
    tour_steps: [
        (
            "Dossiers",
            "Ouvrir un dossier ajoute ici un dossier de photos, et Ajouter un dossier au-dessus de la liste en ajoute un autre. Chacun est listé avec ses sous-dossiers. Cliquez sur l'un d'eux pour afficher ses photos ; faites un clic droit sur un dossier pour le retirer de la liste.",
        ),
        (
            "Barre de filtres",
            "N'affichez que les photos avec une note, un drapeau ou une étiquette de couleur, et changez l'ordre de tri.",
        ),
        (
            "Photos",
            "Les vignettes du dossier. Double-cliquez sur une photo ou appuyez sur Enter pour l'ouvrir dans la Loupe, et faites un clic droit dessus pour en savoir plus.",
        ),
        (
            "Barre d'actions",
            "Agit sur la sélection : notez-la, copiez ou appliquez des ajustements, et utilisez le menu Actions pour mettre des rafales en pile, ajuster automatiquement ou supprimer.",
        ),
        (
            "Ajustements",
            "L'icône des curseurs ouvre les curseurs de tons et de couleurs et vos préréglages enregistrés, pour la photo sélectionnée.",
        ),
        (
            "Infos",
            "L'icône i affiche les métadonnées de la photo : appareil, objectif, exposition, date et lieu de la prise de vue. Appuyez sur I pour les afficher aussi.",
        ),
        (
            "Exporter",
            "L'icône d'export choisit un dossier et une taille, puis exporte les photos sélectionnées en JPEG. Appuyez sur Cmd+Shift+E pour l'ouvrir aussi.",
        ),
        (
            "Plus dans la Loupe",
            "Quand une photo est ouverte, la barre latérale ajoute trois icônes :",
        ),
        (
            "En-tête",
            "Le bouton ? liste les raccourcis clavier, Réglages change le thème, la langue et la taille du texte, et Visite affiche de nouveau ce guide.",
        ),
    ],

    rating_filter: "Note :",
    all: "Toutes",
    at_least_n_stars: "Au moins N étoiles",
    exactly_n_stars: "Exactement N étoiles",
    at_most_n_stars: "Au plus N étoiles",
    show_rated: |cmp, n| format!("Afficher les photos notées {cmp} {n}"),
    unrated_tip: "Afficher seulement les photos sans note",
    flag_filter_label: "Drapeau :",
    show_flagged: |flag| format!("Afficher seulement les photos : {flag}"),
    label_filter_label: "Étiquette :",
    show_labeled: |label| format!("Afficher les photos avec l'étiquette {label}"),
    flag_not_rejected: "Retenues ou sans drapeau",
    flag_picked: "Retenue",
    flag_rejected: "Rejetée",
    flag_unflagged: "Sans drapeau",
    set_flag_tip: |flag| format!("Marquer : {flag}"),
    eyes_closed: "Yeux fermés",
    eyes_closed_tip: "Afficher seulement les photos où quelqu'un a cligné des yeux",
    n_of_m_photos: |n, m| format!("{n} / {m} photo{}", if m > 1 { "s" } else { "" }),
    no_filter_matches: "Aucune photo ne correspond au filtre",
    show_all_photos: "Afficher toutes les photos",
    thumb_unreadable: "Illisible",

    selected_photo: "Photo sélectionnée :",
    selected_n_photos: |n| {
        format!(
            "{n} photo{} sélectionnée{} :",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    score_all: "Tout évaluer",
    score_all_tip: "Évalue chaque photo ici de 0 à 100, membres des piles compris",
    group_all_bursts: "Pile auto par heure de prise de vue",
    group_all_bursts_tip: "Met en pile les photos d'ici prises à moins d'une seconde d'intervalle",
    group_selected_bursts_tip: "Met en pile les photos sélectionnées prises à moins d'une seconde d'intervalle",
    expand_stack: "Déplier la pile",
    collapse_stack: "Replier la pile",
    auto_adjust_all: "Ajuster tout",
    auto_adjust_all_tip: "Règle les curseurs de tons de chaque photo d'après son propre histogramme, membres des piles compris",
    update_score: "Actualiser le score",
    n_selected: |n| format!("{n} sélectionnée{}", if n > 1 { "s" } else { "" }),
    rate_menu: "Noter \u{2605}",
    actions_menu: "Actions",
    open_in_loupe: "Ouvrir dans la Loupe",
    group_into_stack: "Regrouper en pile",
    reset_adjustments: "Réinitialiser les ajustements",
    score_label: "Score :",
    selected_n_photos_title: |n| {
        format!(
            "{n} photo{} sélectionnée{}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    update_scores: "Actualiser les scores",
    clear_rating: "Effacer la note",
    auto_tone: "Ajustement auto",
    auto_tone_selection_tip:
        "Règle les curseurs de tons de chaque photo sélectionnée d'après son propre histogramme",
    score_selection_tip: "Évalue la qualité de chaque photo sélectionnée de 0 à 100",
    quality_of: |v| format!("Qualité {v} sur 100"),
    base_technical: |b| format!("Part de {b} : contrôles techniques seulement (mise au point, exposition, bruit, yeux)"),
    base_aesthetics: |b| format!("Score esthétique Apple : {b}"),
    score_no_breakdown: "Évaluez de nouveau pour voir le détail",
    no_penalties: "Aucun défaut trouvé",
    penalty: |p| {
        use crate::scoring::quality::Penalty::*;
        match p {
            SoftFocus => "Mise au point floue",
            HighlightsClipped => "Hautes lumières écrêtées",
            ShadowsCrushed => "Ombres bouchées",
            Dark => "Sous-exposée",
            Bright => "Surexposée",
            Noisy => "Bruitée",
            EyesClosed => "Yeux fermés",
            Utility => "Ressemble à une capture d'écran ou à un document",
        }
    },
    score_stale: "Modifiée depuis l'évaluation ; évaluez de nouveau pour actualiser",
    sort_by: "Tri :",
    sort_name: "Nom",
    sort_time: "Heure de prise de vue",
    zoom_fit: "Ajuster",
    zoom_tip: "Zoom, de l'ajustement à la fenêtre jusqu'à 6400%",
    zoom_actual: "100% (1:1)",
    sort_time_tip: "Par heure de prise de vue. Les photos sans heure viennent en dernier.",
    sort_modified: "Heure de modification",
    sort_modified_tip: "Par date de dernière modification du fichier.",
    sort_size: "Taille du fichier",
    sort_size_tip: "Par taille de fichier.",
    sort_shutter: "Vitesse d'obturation",
    sort_shutter_tip: "Par vitesse d'obturation. Les photos sans vitesse viennent en dernier.",
    sort_quality: "Score",
    sort_quality_tip: "Par score de qualité. Les photos non évaluées viennent en dernier.",
    sort_rating: "Note",
    sort_rating_tip: "Par note en étoiles. Les photos sans note comptent pour aucune étoile.",
    sort_edited: "Heure d'édition",
    sort_edited_tip: "Par date de dernière retouche ici. Les photos non retouchées viennent en dernier.",
    sort_ascending_tip: "Croissant. Cliquez pour inverser l'ordre.",
    sort_descending_tip: "Décroissant. Cliquez pour inverser l'ordre.",
    copy_settings: "Copier l'ajustement",
    copy_settings_tip: "Copie l'ajustement de cette photo, selon les Options de copie",
    copy_settings_choose_tip: "Choisir ce que Copier l'ajustement copie (Cmd+Shift+C)",
    copy_settings_prompt: "Choisissez ce que Copier l'ajustement transmet aux autres photos. Le recadrage, le redressement et la retouche sont choisis à nouveau à chaque copie.",
    copy_button: "Copier",
    check_all: "Tout cocher",
    check_none: "Tout décocher",
    compare_too_many: "Comparer accepte de 2 à 6 photos",
    effects: "Effets",
    crop_section: "Recadrage",
    apply_settings: "Appliquer l'ajustement",
    apply_settings_needs_copy: "Copiez d'abord l'ajustement d'une photo",
    apply_settings_tip: "Applique l'ajustement copié à la sélection (Cmd+Shift+V)",
    settings_from: |name| format!("de {name}"),
    export_jpg: "Exporter",
    export_jpg_tip: "Choisir où et à quelle taille, puis exporter la sélection en JPG (Cmd+Shift+E)",
    bursts_reading: "Lecture des heures de prise de vue\u{2026}",
    bursts_none: "Aucune rafale trouvée",
    bursts_grouped: |n, photos| match n {
        1 => format!("1 rafale de {photos} photos mise en pile"),
        n => format!("{n} rafales mises en pile, {photos} photos"),
    },
    delete: "Supprimer",
    delete_selection_tip: if WEB {
        "Supprimer définitivement les photos sélectionnées ; irréversible (Delete)"
    } else {
        "Placer les photos sélectionnées dans la Corbeille (Delete)"
    },

    confirm: "Confirmer",
    cancel: "Annuler",
    shortcuts_title: "Raccourcis clavier",
    close: "Fermer",
    help: &[
        HelpSection {
            title: "Navigation",
            rows: &[
                ("Enter or Space", "Ouvrir la photo sélectionnée (dans la bibliothèque)"),
                ("E", "Ouvrir la photo sélectionnée (dans la bibliothèque)"),
                ("G", "Retour à la grille de la bibliothèque"),
                ("F", "Plein écran, la photo seule ; F ou Esc pour quitter (dans l'éditeur)"),
                ("Cmd+Enter", "Diaporama des photos de la grille ; F ou Esc pour l'arrêter"),
                ("Esc", "Retour à la bibliothèque (dans l'éditeur)"),
                ("\u{2190} / \u{2192}", "Photo précédente / suivante (dans l'éditeur)"),
                ("Page Up / Page Down", "Photo précédente / suivante (dans l'éditeur)"),
                (
                    "\u{2190} \u{2192} \u{2191} \u{2193}",
                    "Déplacer la sélection dans la grille de la bibliothèque",
                ),
                ("D", "Ajuster la photo sélectionnée"),
                ("C", "Comparer de 2 à 6 photos sélectionnées, ou la pile sélectionnée"),
                ("I", "Afficher ou masquer les infos de la photo dans le panneau de droite"),
                ("F6 / Shift+F6", "Zone suivante, ou commande suivante dans une zone ouverte"),
                ("Tab / Shift+Tab", "Passer d'un élément à l'autre dans la zone active"),
                ("Cmd+O", "Ouvrir un dossier"),
                ("?", "Afficher ou masquer cette aide"),
                (
                    "Cmd+,",
                    if SHOW_AUTOTONE_CENTERING && RAW_SETTING {
                        "Réglages : thème, langue, courbe de tons RAW et Ajustement auto"
                    } else if SHOW_AUTOTONE_CENTERING {
                        "Réglages : thème, langue et Ajustement auto"
                    } else if RAW_SETTING {
                        "Réglages : thème, langue et courbe de tons RAW"
                    } else {
                        "Réglages : thème et langue"
                    },
                ),
                ("Alt+= / Alt+-", "Texte plus grand / plus petit"),
            ],
        },
        HelpSection {
            title: "Sélection",
            rows: &[
                ("Cmd+A", "Tout sélectionner dans le dossier actuel (dans la bibliothèque)"),
                ("Shift+Click", "Sélectionner une plage"),
                ("Cmd+Click", "Ajouter ou retirer une photo de la sélection"),
                ("Shift+arrows", "Étendre la sélection (bibliothèque)"),
                ("Cmd+D", "Tout désélectionner"),
                ("Cmd+G", "Mettre la sélection en pile"),
                ("Cmd+Shift+G", "Dissocier les piles sélectionnées"),
                (
                    "Shift+Delete",
                    "Supprimer les piles sélectionnées, ou les dissocier en gardant les photos",
                ),
            ],
        },
        HelpSection {
            title: "Noter, marquer et étiqueter",
            rows: &[
                ("0 1 2 3 4 5", "Définir la note de 0 à 5 étoiles"),
                ("Shift+0-5", "Définir la note et passer à la photo suivante"),
                ("P", "Retenir"),
                ("X", "Rejeter"),
                ("U", "Retirer le drapeau"),
                ("`", "Retenir, ou retirer le drapeau d'une photo retenue"),
                ("6 7 8 9", "Étiquette rouge, jaune, verte ou bleue ; de nouveau pour l'effacer"),
            ],
        },
        HelpSection {
            title: "Zoom et déplacement (dans l'éditeur)",
            rows: &[
                ("Space or Z", "Changer de zoom : ajusté, 2x ajusté, 100%"),
                ("Cmd++ or Cmd+=", "Zoom avant (par pas de 20%)"),
                ("Cmd+-", "Zoom arrière (par pas de 20%)"),
                ("\u{2191} / \u{2193}", "Zoom avant / arrière (par pas de 10%)"),
                ("Scroll", "Zoomer sous le pointeur"),
                ("Drag", "Déplacer"),
                ("Space+Drag", "Déplacer"),
                ("Shift+Scroll", "Déplacer latéralement"),
                ("Alt+Scroll", "Déplacer vers le haut et le bas"),
            ],
        },
        HelpSection {
            title: "Édition",
            rows: &[
                ("Cmd+Z", "Annuler le dernier changement de développement"),
                ("Cmd+[ / Cmd+]", "Faire pivoter l'image de -90 / +90 degrés"),
                ("R", "Recadrer"),
                ("Y", "Comparer avant / après (dans l'éditeur)"),
                ("M", "Masque dégradé linéaire"),
                ("Shift+M", "Masque dégradé radial"),
                ("Cmd+Shift+E", "Ouvrir ou fermer le formulaire d'export"),
                ("Cmd+U", "Ajuster automatiquement la sélection"),
                ("V", "Noir et blanc, ou retour à la couleur"),
                ("Cmd+Shift+C", "Copier l'ajustement de cette photo"),
                ("Cmd+Shift+V", "Coller l'ajustement sur la sélection"),
                ("Double-click slider", "Réinitialiser le curseur"),
                (
                    "Delete",
                    if WEB {
                        "Supprimer définitivement ; irréversible"
                    } else {
                        "Placer dans la Corbeille"
                    },
                ),
            ],
        },
        HelpSection {
            title: "Recadrage",
            rows: &[
                ("Enter or R", "Appliquer le recadrage"),
                ("Esc", "Annuler le recadrage"),
                ("Cmd+[ / Cmd+]", "Faire pivoter de -90 / +90 degrés"),
            ],
        },
        HelpSection {
            title: "Choisir le représentant",
            rows: &[
                ("Click or drag the photo", "Déplacer la zone de mise au point"),
                ("Shift+scroll on the area", "Redimensionner la zone de mise au point"),
                ("Drag a tile", "Déplacer la zone de mise au point"),
                ("Cmd+Click / Shift+Click a tile", "Choisir d'autres vignettes"),
            ],
        },
        HelpSection {
            title: "Sélection du sujet",
            rows: if SUBJECT_KEYS {
                &[
                    ("O", "Afficher ou masquer la sélection du sujet"),
                    ("Shift+O", "Inverser la sélection du sujet"),
                ]
            } else {
                &[]
            },
        },
        HelpSection {
            title: "Masque au pinceau",
            rows: &[
                ("K", "Pinceau activé / désactivé ; peint le masque au pinceau choisi"),
                ("Drag", "Peindre ; chaque trait est une étape d'annulation"),
                ("Alt+Drag", "Effacer"),
                ("[ / ]", "Taille du pinceau"),
                ("Scroll", "Taille du pinceau"),
                ("Shift+[ / ]", "Contour progressif du pinceau"),
                ("Shift+Scroll", "Contour progressif du pinceau"),
                ("Esc", "Poser le pinceau"),
            ],
        },
        HelpSection {
            title: "Retouche",
            rows: &[
                ("Q", "Retouche activée / désactivée"),
                ("[ / ]", "Taille du pinceau"),
                ("Scroll", "Taille du pinceau"),
                ("Shift+[ / ]", "Contour progressif du pinceau"),
                ("Shift+Scroll", "Contour progressif du pinceau"),
                ("O", "Afficher ou masquer les taches"),
                ("Shift+O", "Sélectionner la tache suivante"),
                ("Delete", "Supprimer la tache sélectionnée"),
                ("Cmd+Z", "Annuler la dernière tache"),
                ("Esc", "Quitter la retouche"),
            ],
        },
    ],

    confirm_apply_settings: |n| {
        format!(
            "Appliquer l'ajustement copié à {n} photo{} ?",
            if n > 1 { "s" } else { "" }
        )
    },
    confirm_auto_tone: |n| {
        format!(
            "Ajuster automatiquement {n} photo{} ?",
            if n > 1 { "s" } else { "" }
        )
    },
    confirm_delete: if WEB {
        |n| {
            format!(
                "Supprimer définitivement {n} photo{} ? Cette action est irréversible.",
                if n > 1 { "s" } else { "" }
            )
        }
    } else {
        |n| {
            format!(
                "Placer {n} photo{} dans la Corbeille ?",
                if n > 1 { "s" } else { "" }
            )
        }
    },
    confirm_delete_groups: if WEB {
        |n, g| {
            format!(
                "Supprimer définitivement {n} photo{} ? Cela inclut toutes les photos de {g} pile{}. Cette action est irréversible.",
                if n > 1 { "s" } else { "" },
                if g > 1 { "s" } else { "" }
            )
        }
    } else {
        |n, g| {
            format!(
                "Placer {n} photo{} dans la Corbeille ? Cela inclut toutes les photos de {g} pile{}.",
                if n > 1 { "s" } else { "" },
                if g > 1 { "s" } else { "" }
            )
        }
    },
    delete_group_title: "Supprimer la pile",
    delete_group_prompt: |n, g| {
        if g == 1 {
            format!(
                "Cette pile contient {n} photo{}. Dissocier les conserve comme photos individuelles.",
                if n > 1 { "s" } else { "" }
            )
        } else {
            format!(
                "Ces {g} piles contiennent {n} photo{}. Dissocier les conserve comme photos individuelles.",
                if n > 1 { "s" } else { "" }
            )
        }
    },
    remove_group: "Dissocier",
    trash_group_photos: if WEB {
        |n| format!("Supprimer {n} photo{}", if n > 1 { "s" } else { "" })
    } else {
        |n| {
            format!(
                "Placer {n} photo{} dans la Corbeille",
                if n > 1 { "s" } else { "" }
            )
        }
    },
    bulk_delete: if WEB {
        "Supprimer les photos"
    } else {
        "Placer dans la Corbeille"
    },

    develop: "Développement",
    tab_sliders: "Ajustements",
    tab_crop: "Recadrage",
    tab_masks: "Masques/retouche",
    crop_rotate: "Rotation",
    crop_left: "Gauche",
    crop_right: "Droite",
    crop_size: "Taille",
    crop_overlay: "Incrustation",
    crop_thirds: "Tiers",
    crop_grid: "Grille",
    crop_golden: "Nombre d'or",
    crop_diagonal: "Diagonale",
    crop_spiral: "Spirale",
    crop_overlay_none: "Aucune",
    crop_aspect: "Format",
    crop_original: "Original",
    crop_custom: "Personnalisé",
    crop_orientation: "Orientation",
    crop_horizontal: "Horizontale",
    crop_vertical: "Verticale",
    crop_straighten: "Redresser",
    crop_straighten_tool: "Outil Redresser",
    crop_straighten_hint: "Tracez le long d'une ligne qui doit être horizontale ou verticale, puis appuyez sur Enter.",
    presets: "Préréglages",
    save_preset_tip: "Enregistrer ces réglages comme préréglage",
    import_lr_presets: "Importer depuis Lightroom…",
    import_lr_presets_tip: "Importer des fichiers de préréglage Lightroom .xmp",
    xmp_picker_title: "Choisir des préréglages Lightroom",
    xmp_filter_name: "Préréglage Lightroom",
    preset_import_note: |list| format!("Réglages Lightroom sans équivalent ici : {list}"),
    preset_monochrome_note: "Noir et blanc approché par Saturation -100",
    no_presets: "Aucun préréglage pour le moment",
    apply_preset_tip: "Appliquer aux photos sélectionnées",
    preset_actions_tip: "Renommer ou supprimer",
    search_presets: "Rechercher des préréglages",
    my_presets: "Mes préréglages",
    no_matching_presets: "Aucun préréglage ne correspond",
    save_current_as_preset: "Enregistrer comme préréglage…",
    add_preset_folder: "Ajouter un dossier de préréglages…",
    add_preset_folder_tip: "Liste les préréglages Lightroom .xmp d'un dossier, lus sur place",
    preset_folder_picker_title: "Choisir un dossier de préréglages Lightroom",
    preset_folders: "Dossiers de préréglages",
    remove: "Retirer",
    reading_presets: |name| format!("Lecture de {name}…"),
    preset_folder_unavailable: |name| format!("Impossible de lire {name}"),
    preset_folder_empty: |name| format!("Aucun préréglage dans {name}"),
    preset_folder_needs_permission: |name| format!("Cliquez pour autoriser l'accès à {name}"),
    preset_folder_truncated: |name, n| format!("Seuls les {n} premiers fichiers de {name} sont listés"),
    presets_skipped: |n| {
        format!(
            "{n} fichier{} ignoré{}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    presets_skipped_tip: "Profils, rendus et fichiers .xmp qui ne sont pas des préréglages de développement ou n'ont pas pu être lus",
    preset_folder_add_failed: |e| format!("Impossible d'ajouter le dossier de préréglages : {e}"),
    save_preset_title: "Enregistrer le préréglage",
    rename_preset_title: "Renommer le préréglage",
    preset_name_hint: "Nom du préréglage",
    preset_name_label: "Nom",
    save: "Enregistrer",
    rename: "Renommer",
    preset_default_name: |n| format!("Préréglage {n}"),
    confirm_delete_preset: |name| format!("Supprimer le préréglage {name} ?"),
    reset: "Réinitialiser",
    auto_tone_tip: "Règle les curseurs de tons d'après l'histogramme de chaque photo sélectionnée (Cmd+U)",
    black_and_white: "N&B",
    black_and_white_tip: "Passer les photos sélectionnées en noir et blanc, ou revenir à la couleur (V)",
    touch_up: "Retouche",
    brush_size: "Taille",
    brush_size_tip: "[ / ] ou la molette",
    feather: "Contour progressif",
    feather_tip: "Shift+[ / Shift+] ou Shift+molette",
    opacity: "Opacité",
    opacity_tip: "Quelle part de la correction recouvre la photo, pour les taches ajoutées à partir de maintenant",
    spots: "Taches",
    pick_gray: "Pipette gris",
    pick_gray_tip: "Cliquez sur un pixel gris neutre de l'image",
    white_balance: "Balance des blancs",
    tone: "Tonalité",
    presence: "Présence",
    detail: "Détail",
    optics: "Optique",
    vignette: "Vignetage",
    grain: "Grain",
    curve: "Courbe",
    point_curve: "Courbe de points",
    curve_linear: "Linéaire",
    curve_medium_contrast: "Contraste moyen",
    curve_strong_contrast: "Contraste fort",
    curve_custom: "Personnalisée",
    curve_rgb: "RVB",
    curve_red: "Rouge",
    curve_green: "Vert",
    curve_blue: "Bleu",
    color_mixer: "Mixage couleur",
    gray_mix: "Mixage N&B",
    copy_color: "Couleur",
    color_grading: "Étalonnage couleur",
    midtones: "Tons moyens",
    global: "Global",
    blending: "Fusion",
    balance: "Équilibre",
    mix_hue: "Teinte",
    mix_luminance: "Luminance",
    bands: [
        "Rouge",
        "Orange",
        "Jaune",
        "Vert",
        "Aigue-marine",
        "Bleu",
        "Violet",
        "Magenta",
    ],
    remove_ca: "Supprimer l'aberration chromatique",
    remove_ca_tip: "Mesure les franges rouges et bleues sur les contours très contrastés et les aligne sur le vert",
    no_photo_selected: "Aucune photo sélectionnée",
    multiple_photos_selected: "Plusieurs photos sélectionnées",
    step_big_down: "Baisse la valeur de chaque photo d'un grand pas",
    step_down: "Baisse la valeur de chaque photo d'un petit pas",
    step_reset: "Réinitialise toutes les photos sélectionnées",
    step_up: "Augmente la valeur de chaque photo d'un petit pas",
    step_big_up: "Augmente la valeur de chaque photo d'un grand pas",
    temp: "Température",
    tint: "Teinte",
    exposure: "Exposition",
    contrast: "Contraste",
    highlights: "Hautes lumières",
    shadows: "Ombres",
    whites: "Blancs",
    blacks: "Noirs",
    texture: "Texture",
    clarity: "Clarté",
    dehaze: "Correction de la brume",
    vibrance: "Vibrance",
    saturation: "Saturation",
    denoise: "Réduction du bruit",
    sharpening: "Netteté",
    sharpen_radius: "Rayon",
    sharpen_detail: "Détail",
    sharpen_masking: "Masquage",
    amount: "Quantité",
    midpoint: "Point médian",
    roundness: "Arrondi",
    grain_size: "Taille",
    roughness: "Rugosité",

    before: "Avant",
    after: "Après",
    invert: "Inverser",
    invert_tip: "Met en évidence l'arrière-plan au lieu du sujet",
    show_selection: "Afficher la sélection",
    selection_pending: "Sélection\u{2026}",
    no_subject: "Aucun sujet",
    show_selection_tip: "Entoure le sujet que Vision trouve dans cette photo",
    capture_date: |year, month, day, hour, minute| {
        format!("{day:02}/{month:02}/{year} {hour:02}:{minute:02}")
    },

    deleting: if WEB {
        |done, total| format!("Suppression de {done}/{total}\u{2026}")
    } else {
        |done, total| format!("Mise à la Corbeille de {done}/{total}\u{2026}")
    },
    deleted: if WEB {
        |n| {
            format!(
                "{n} photo{} supprimée{} définitivement",
                if n > 1 { "s" } else { "" },
                if n > 1 { "s" } else { "" }
            )
        }
    } else {
        |n| {
            format!(
                "{n} photo{} placée{} dans la Corbeille",
                if n > 1 { "s" } else { "" },
                if n > 1 { "s" } else { "" }
            )
        }
    },
    deleted_partial: if WEB {
        |n, total, e| format!("{n}/{total} supprimées définitivement \u{2014} dernière erreur : {e}")
    } else {
        |n, total, e| format!("{n}/{total} placées dans la Corbeille \u{2014} dernière erreur : {e}")
    },
    delete_in_progress: "Une autre suppression est encore en cours\u{2026}",
    group_refused_loading: "Les piles de ce dossier sont encore en cours de chargement. Réessayez dans un instant.",
    group_name_unsaveable: "Un nom de fichier ici ne peut pas être enregistré dans une pile.",
    delete_no_handle: |dir| format!("Impossible de supprimer les photos : aucun accès au dossier {dir}"),
    copied_settings_from: |name| format!("Ajustement copié depuis {name}"),
    applied_settings: |n| {
        format!(
            "Ajustement appliqué à {n} photo{}",
            if n > 1 { "s" } else { "" }
        )
    },
    saved_preset: |name| format!("Préréglage {name} enregistré"),
    lr_import_status: |ok, failed, first| match (ok, failed) {
        (ok, 0) => format!(
            "{ok} préréglage{} importé{}",
            if ok > 1 { "s" } else { "" },
            if ok > 1 { "s" } else { "" }
        ),
        (0, _) => first.to_string(),
        (ok, failed) => format!(
            "{ok} préréglage{} importé{}, {failed} ignoré{} ({first})",
            if ok > 1 { "s" } else { "" },
            if ok > 1 { "s" } else { "" },
            if failed > 1 { "s" } else { "" }
        ),
    },
    lr_import_failed: |file, reason| format!("Impossible d'importer {file} : {reason}"),
    applied_preset: |name, n| {
        format!(
            "{name} appliqué à {n} photo{}",
            if n > 1 { "s" } else { "" }
        )
    },
    renamed_preset: |name| format!("Préréglage renommé en {name}"),
    deleted_preset: |name| format!("Préréglage {name} supprimé"),
    cleared_rating: |n| {
        format!(
            "Note effacée sur {n} photo{}",
            if n > 1 { "s" } else { "" }
        )
    },
    rated: |n, stars| {
        format!(
            "{n} photo{} notée{} \u{2605}{stars}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    flagged: |n, flag| {
        format!(
            "{n} photo{} marquée{} : {flag}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    export_title: |n| {
        if n <= 1 {
            format!("Exporter {n} photo")
        } else {
            format!("Exporter {n} photos")
        }
    },
    export_destination: "Destination",
    export_to_folder: "Dossier",
    export_to_immich: "Immich",
    export_exports_subfolder: "Sous-dossier Exports",
    export_chosen_folder: "Autre dossier",
    export_choose_folder: "Choisir\u{2026}",
    export_size: "Taille",
    export_folder: "Emplacement",
    export_output: "Sortie",
    immich_account: "Compte",
    export_size_full: "Pleine résolution",
    export_size_long_edge: |px| format!("{px} px, grand côté"),
    export_file: "Fichier",
    export_format: "Format",
    export_format_tiff16: "TIFF (16 bits)",
    export_format_webp: "WebP (sans perte)",
    export_limit: "Taille du fichier",
    export_limit_none: "Sans limite",
    export_limit_kb: |kb| {
        if kb >= 1024 && kb % 1024 == 0 {
            format!("Jusqu'à {} Mo", kb / 1024)
        } else {
            format!("Jusqu'à {kb} Ko")
        }
    },
    export_jpeg_preset: "Encodage",
    export_jpeg_fastest: "Le plus rapide",
    export_jpeg_baseline: "Baseline",
    export_jpeg_progressive: "Progressif",
    export_jpeg_smallest: "Le plus petit",
    export_jpeg_preset_hint: "Le plus rapide encode plusieurs fois plus vite. Les autres produisent des fichiers 10 à 20 % plus petits.",
    export_naming: "Nom du fichier",
    export_naming_hint: "{name} est le nom de la photo et {seq} son numéro dans cet export.",
    export_naming_example: |name| format!("Par exemple : {name}"),
    export_metadata: "Métadonnées",
    export_metadata_include: "Inclure",
    export_keep_location: "Lieu (GPS)",
    export_keep_identity: "Numéros de série de l'appareil et propriétaire",
    export_keep_hint: "La date de prise de vue, l'appareil, l'objectif et l'exposition sont toujours inclus. Les légendes et les commentaires ne le sont jamais.",
    export_copyright: "Copyright",
    export_copyright_hint: "Écrit dans chaque export à la place de celui de la photo. Laissez vide pour garder celui de la photo.",
    export_sharpen: "Netteté pour",
    export_sharpen_off: "Aucune",
    export_sharpen_screen: "Écran",
    export_sharpen_matte: "Papier mat",
    export_sharpen_glossy: "Papier brillant",
    export_sharpen_low: "Faible",
    export_sharpen_standard: "Standard",
    export_sharpen_high: "Élevée",
    export_run: "Exporter",
    export_needs_immich: "Connectez-vous d'abord à un serveur Immich",
    immich_server_url: "URL du serveur",
    immich_url_example: "Exemple : https://immich.gumnut.ai",
    immich_plain_http: "http:// n'est pas chiffré : votre clé d'API et vos photos circulent en clair sur le réseau. Utilisez https:// si le serveur le propose.",
    immich_native_only: "L'envoi vers Immich n'est disponible que dans l'application de bureau. Les navigateurs ne peuvent pas se connecter à un serveur Immich.",
    immich_api_key: "Clé d'API",
    immich_connect: "Se connecter",
    immich_connecting: "Connexion\u{2026}",
    immich_connected_as: |name| format!("Connecté en tant que {name}"),
    immich_disconnect: "Se déconnecter",
    immich_key_storage: if cfg!(target_os = "macos") {
        "La clé est conservée dans votre trousseau."
    } else {
        "La clé est conservée dans un fichier que vous seul pouvez lire."
    },
    uploading: |done, total| format!("Envoi de {done}/{total}\u{2026}"),
    uploaded: |n, dup| match dup {
        0 => format!(
            "{n} photo{} envoyée{}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        ),
        _ => format!(
            "{n} photo{} envoyée{}, dont {dup} déjà sur le serveur",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        ),
    },
    uploaded_partial: |ok, total, e| format!("{ok}/{total} envoyées \u{2014} dernière erreur : {e}"),
    ratings_not_set: |summary, n, e| format!("{summary} \u{2014} note non définie sur {n} : {e}"),
    immich_album: "Album",
    album_none: "Aucun album",
    album_new: "Nouvel album\u{2026}",
    album_name: "Nom de l'album",
    album_name_needed: "Nommez d'abord le nouvel album",
    albums_failed: |e| format!("Impossible de charger les albums : {e}"),
    adding_to_album: |summary, name| format!("{summary} \u{2014} ajout à {name}\u{2026}"),
    added_to_album: |summary, name| format!("{summary} \u{2014} ajouté à {name}"),
    album_failed: |summary, e| format!("{summary} \u{2014} non ajouté à l'album : {e}"),
    export_nothing_selected: "Export : rien n'est sélectionné",
    export_in_progress: "Un export est déjà en cours\u{2026}",
    export_edits_loading: "Export : les retouches de ce dossier sont encore en cours de chargement, réessayez dans un instant\u{2026}",
    export_no_handle: "Export : aucun accès au dossier actuel",
    export_no_folder: |e| format!("Échec de l'export : impossible de créer le dossier Exports : {e}"),
    exporting: |done, total| format!("Export de {done}/{total}\u{2026}"),
    exported: |n| {
        format!(
            "{n} photo{} exportée{}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    exported_partial: |ok, total, e| format!("{ok}/{total} exportées \u{2014} dernière erreur : {e}"),
    auto_tone_needs_load: "L'ajustement auto demande que la photo finisse de se charger",
    auto_tone_applied: "Ajustement auto appliqué",
    remove_ca_measuring: "Mesure de l'aberration chromatique…",
    remove_ca_measuring_n: |n, total| format!("Mesure de l'aberration chromatique… {n} sur {total}"),
    remove_ca_applied: "Aberration chromatique supprimée",
    remove_ca_failed: "Impossible de décoder la photo pour mesurer l'aberration chromatique",
    auto_tone_waits_for_edits: "L'ajustement auto démarrera quand les retouches de ce dossier seront chargées\u{2026}",
    auto_tone_stopped: |done, total| format!("Ajustement auto arrêté à {done}/{total}"),
    auto_tone_applied_n: |n| {
        format!(
            "Ajustement auto appliqué à {n} photo{}",
            if n > 1 { "s" } else { "" }
        )
    },
    auto_tone_progress: |done, total| format!("Ajustement auto {done}/{total}\u{2026}"),
    scoring_progress: |done, total| format!("Évaluation {done}/{total}\u{2026}"),
    scoring_stopped: |done, total| format!("Évaluation arrêtée à {done}/{total}"),
    scored_n: |n| {
        format!(
            "{n} photo{} évaluée{}",
            if n > 1 { "s" } else { "" },
            if n > 1 { "s" } else { "" }
        )
    },
    scored_partial: |ok, total| format!("{ok}/{total} évaluées ; les autres n'ont pas pu être lues"),
    touch_up_limit: "La retouche accepte jusqu'à 64 taches",
    mask_limit: "Une photo accepte jusqu'à 16 masques",
    masks: "Masques",
    linear_gradient: "Linéaire",
    radial_gradient: "Radial",
    linear_gradient_tip: "Dégradé linéaire : faites glisser sur la photo pour le tracer (M)",
    radial_gradient_tip: "Dégradé radial : faites glisser depuis son centre sur la photo (Shift+M)",
    brush_mask: "Pinceau",
    brush_mask_tip: "Pinceau : peignez le masque sur la photo (K). Maintenez Alt pour effacer",
    brush_paint_hint: "Peignez sur la photo. Maintenez Alt pour effacer",
    flow: "Flux",
    flow_tip: "Quantité ajoutée par passage. Les passages sur un même endroit s'additionnent",
    brush_erase: "Gomme",
    brush_erase_tip: "Peignez pour retirer du masque, comme en maintenant Alt",
    brush_point_limit: "Les traits de pinceau d'une photo acceptent jusqu'à 8 000 points",
    brush_stroke_limit: "Un masque au pinceau accepte jusqu'à 512 traits ; + Pinceau en commence un autre",
    mask_invert: "Inverser",
    mask_draw_hint: "Faites glisser sur la photo pour tracer le masque",
    subject_mask: "Sujet",
    sky_mask: "Ciel",
    detect_nothing_found: |what| format!("Aucun élément « {what} » trouvé dans cette photo"),
    detect_failed: |what, why| format!("Échec de la détection de « {what} » : {why}"),
    subject_mask_tip: "Sujet : un masque sur la personne ou le sujet principal, détecté pour cette photo",
    sky_mask_tip: "Ciel : un masque sur le ciel, détecté pour cette photo",
    detect_needs_mac: "La détection du sujet demande macOS",
    detect_unavailable: "Pas encore disponible dans cette version",
    detect_file_missing: "Le fichier de détection de ce masque est absent de .lightphotos/masks",
    detect_read_failed: |what, why| format!("Le fichier du masque « {what} » n'a pas pu être lu : {why}"),
    mask_detecting: "Détection\u{2026}",
    mask_retry: "Réessayer",
    export_masks_detecting: "Export : les masques sont encore en cours de détection, réessayez dans un instant\u{2026}",
    touch_up_needs_full: "La retouche demande une image en pleine résolution",
    wb_no_image: "Aucune image chargée où prélever",
    wb_pick_brighter: "Choisissez un pixel plus clair et moins saturé pour la balance des blancs",
    open_folder_failed: |e| format!("Impossible d'ouvrir le dossier : {e}"),
    catalog_fallback: |e| format!("Impossible d'ouvrir le dernier catalogue, le catalogue par défaut est donc ouvert : {e}"),
    catalog_open_failed: |e| format!("Impossible d'ouvrir le catalogue : {e}"),
    catalog_save_failed: |e| format!("Impossible d'enregistrer le catalogue : {e}"),
    preset_folders_unreadable: |e| format!("Impossible de lire les dossiers de préréglages : {e}"),
    catalog_create_failed: |e| format!("Impossible de créer le catalogue : {e}"),
    catalog_tip: "Catalogue : les dossiers listés ici, et une copie de leurs notes, drapeaux et étiquettes de couleur",
    new_catalog: "Nouveau catalogue\u{2026}",
    open_catalog: "Ouvrir un catalogue\u{2026}",
    catalog_save_title: "Nouveau catalogue",
    catalog_untitled: "Sans titre",
    catalog_filter_name: "Catalogue LightPhotos",
    catalog_picker_title: "Choisir un catalogue (un dossier .lightphotos)",
    catalog_name_label: "Nom",
    catalog_name_hint: "Nom du catalogue",
    create: "Créer",
    preview_failed: |path| format!("Impossible de charger l'aperçu de la Loupe pour {path}"),
    group_page: |first, last, n| format!("{first}–{last} sur {n}"),
    tile_load: "Charger les photos :",
    tile_speed: "Rapidement",
    tile_full: "Complètement (plus lent)",
    set_as_rep: "Définir comme représentante",
    representative: "Représentante",
    view_compare: "Comparer les photos",
    view_compare_needs_group: "Comparer les photos : sélectionnez de 2 à 6 photos, ou une photo d'une pile",
    folder_handle_missing: |dir| format!("Impossible d'ouvrir {dir} \u{2014} accès au dossier manquant"),
    list_folder_failed: |dir, e| format!("Impossible de lister {dir} : {e}"),
    edits_save_failed: |e| format!("Échec de l'enregistrement des notes et des retouches : {e}"),
    held_by_app: |pid| format!("une autre fenêtre LightPhotos (pid {pid})"),
    held_by_mcp: |pid| format!("lightphotos-mcp (pid {pid})"),
    held_by_other: "un autre programme",
    folder_read_only: |who| {
        format!("Lecture seule : {who} a ce dossier ouvert. Les changements faits ici ne sont pas enregistrés. Fermez-le là-bas, puis rouvrez ce dossier.")
    },
    folder_cant_lock: "Lecture seule : ce dossier ne peut pas être verrouillé pour l'enregistrement, donc les changements faits ici ne sont pas enregistrés. Il est peut-être en lecture seule ou demande une autorisation. Rouvrez-le pour réessayer.",
    folder_no_access: "Lecture seule : LightPhotos n'a pas accès à ce dossier, donc les changements faits ici ne sont pas enregistrés. Rouvrez-le.",
    folder_lockless: "Ce dossier ne peut pas être verrouillé : ne laissez pas un lightphotos-mcp sans interface le modifier tant qu'il est ouvert ici.",
    folder_opening: "Ce dossier est encore en cours d'ouverture. Réessayez dans un instant.",
    presets_load_failed: |e| format!("Impossible de lire vos préréglages : {e}"),
    presets_locked:
        "Les préréglages sont verrouillés car le fichier enregistré n'a pas pu être lu. Déplacez-le ou réparez-le, puis relancez l'application.",
    presets_save_failed: |e| format!("Échec de l'enregistrement des préréglages : {e}"),

    menu: MenuStrings {
        about: "À propos de LightPhotos",
        settings: "Réglages\u{2026}",
        hide: "Masquer LightPhotos",
        hide_others: "Masquer les autres",
        show_all: "Tout afficher",
        quit: "Quitter LightPhotos",
        file: "Fichier",
        new_catalog: "Nouveau catalogue\u{2026}",
        open_catalog: "Ouvrir un catalogue\u{2026}",
        open_folder: "Ouvrir un dossier\u{2026}",
        export: "Exporter\u{2026}",
        edit: "Édition",
        undo: "Annuler",
        cut: "Couper",
        copy: "Copier",
        paste: "Coller",
        select_all: "Tout sélectionner",
        deselect_all: "Tout désélectionner",
        copy_settings: "Copier l'ajustement…",
        paste_settings: "Coller l'ajustement",
        photo: "Photo",
        rate: "Noter",
        no_rating: "Aucune note",
        set_flag: "Définir le drapeau",
        flag_picked: "Retenue",
        flag_unflagged: "Sans drapeau",
        flag_rejected: "Rejetée",
        color_label: "Étiquette de couleur",
        label_red: "Rouge",
        label_yellow: "Jaune",
        label_green: "Vert",
        label_blue: "Bleu",
        label_purple: "Violet",
        auto_tone: "Ajustement auto",
        auto_tone_selection: "Ajuster automatiquement la sélection",
        black_and_white: "Noir et blanc",
        score_photos: "Évaluer la qualité",
        rotate_left: "Pivoter à gauche",
        rotate_right: "Pivoter à droite",
        move_to_trash: "Placer dans la Corbeille",
        group_selected: "Regrouper en pile",
        group_bursts: "Pile auto par heure de prise de vue",
        ungroup: "Dissocier",
        delete_group: "Supprimer la pile\u{2026}",
        view: "Présentation",
        grid: "Grille",
        loupe: "Loupe",
        develop: "Ajustements",
        crop: "Recadrage",
        compare_stack: "Comparer les photos",
        info_panel: "Panneau d'infos",
        before_after: "Avant / après",
        toggle_zoom: "Basculer le zoom",
        zoom_in: "Zoom avant",
        zoom_out: "Zoom arrière",
        bigger_text: "Texte plus grand",
        smaller_text: "Texte plus petit",
        window: "Fenêtre",
        minimize: "Placer dans le Dock",
        zoom: "Zoom",
        help: "Aide",
        keyboard_shortcuts: "Raccourcis clavier",
    },
};
