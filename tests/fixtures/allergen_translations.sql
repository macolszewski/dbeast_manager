BEGIN;

WITH messages (translation_key, pl, en, de) AS (
    VALUES
    ('panel.allergens.label',
     'Alergeny',
     'Allergens',
     'Allergene'),

    ('panel.allergens.help.label',
     'Zaznacz alergeny, które zawiera produkt.',
     'Select the allergens contained in the product.',
     'Wähle die Allergene aus, die das Produkt enthält.'),

    ('panel.allergens.pending.label',
     'Trwa przetwarzanie alergenów…',
     'Processing allergens…',
     'Allergene werden verarbeitet…'),

    ('panel.allergens.load_error.label',
     'Nie udało się pobrać listy alergenów.',
     'Could not load the allergen list.',
     'Die Allergenliste konnte nicht geladen werden.'),

    ('panel.allergens.save_error.label',
     'Nie udało się zapisać alergenów. Przywrócono poprzedni wybór. Spróbuj ponownie.',
     'Could not save allergens. The previous selection was restored. Try again.',
     'Die Allergene konnten nicht gespeichert werden. Die vorherige Auswahl wurde wiederhergestellt. Versuche es erneut.'),

    ('panel.allergens.retry.label',
     'Spróbuj ponownie',
     'Try again',
     'Erneut versuchen'),

    ('menuitem.allergen.gluten.label',
     'gluten', 'gluten', 'Gluten'),

    ('menuitem.allergen.crustaceans.label',
     'skorupiaki', 'crustaceans', 'Krebstiere'),

    ('menuitem.allergen.eggs.label',
     'jaja', 'eggs', 'Eier'),

    ('menuitem.allergen.fish.label',
     'ryby', 'fish', 'Fisch'),

    ('menuitem.allergen.peanuts.label',
     'orzeszki ziemne', 'peanuts', 'Erdnüsse'),

    ('menuitem.allergen.soybeans.label',
     'soja', 'soybeans', 'Soja'),

    ('menuitem.allergen.milk.label',
     'mleko', 'milk', 'Milch'),

    ('menuitem.allergen.nuts.label',
     'orzechy', 'nuts', 'Schalenfrüchte'),

    ('menuitem.allergen.celery.label',
     'seler', 'celery', 'Sellerie'),

    ('menuitem.allergen.mustard.label',
     'gorczyca', 'mustard', 'Senf'),

    ('menuitem.allergen.sesame.label',
     'sezam', 'sesame', 'Sesam'),

    ('menuitem.allergen.sulphites.label',
     'dwutlenek siarki i siarczyny',
     'sulphur dioxide and sulphites',
     'Schwefeldioxid und Sulfite'),

    ('menuitem.allergen.lupin.label',
     'łubin', 'lupin', 'Lupinen'),

    ('menuitem.allergen.molluscs.label',
     'mięczaki', 'molluscs', 'Weichtiere')
)
INSERT INTO public.translations_translation (
    translation_key,
    language_code,
    translation_value,
    last_change
)
SELECT
    m.translation_key,
    t.language_code,
    t.translation_value,
    CURRENT_TIMESTAMP
FROM messages AS m
CROSS JOIN LATERAL (
    VALUES
        ('pl', m.pl),
        ('en', m.en),
        ('de', m.de)
) AS t(language_code, translation_value)
ON CONFLICT (translation_key, language_code)
DO UPDATE SET
    translation_value = EXCLUDED.translation_value,
    last_change = EXCLUDED.last_change;

COMMIT;
