//! Équipage tab: a standalone crew planner - roster, level, equipment and training per member,
//! with the resulting stats. Independent from captured battles; the plan persists to
//! `AppPaths::crew_plan()` (the app's data folder, see `pss_store::paths`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use eframe::egui;
use pss_designs::prestige::{self, CharacterPrestiges};
use pss_designs::{CharacterDesignXml, ItemDesignXml};
use pss_model::SpecialAbilityType;
use pss_stats::training::{self, CrewStat, EquipmentSlot, Training};
use pss_store::paths::app_paths;

use super::{palette, special_ability_display_name};

const MAX_SEARCH_RESULTS: usize = 40;
const RARITIES: [&str; 7] = [
    "Common",
    "Elite",
    "Unique",
    "Epic",
    "Hero",
    "Special",
    "Legendary",
];

/// Everything persisted to `AppPaths::crew_plan()`: one roster per game account.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct CrewPlan {
    accounts: Vec<CrewAccount>,
    current: usize,
}

impl Default for CrewPlan {
    fn default() -> Self {
        Self {
            accounts: vec![CrewAccount::named("Compte 1")],
            current: 0,
        }
    }
}

impl CrewPlan {
    fn current(&self) -> &CrewAccount {
        &self.accounts[self.current]
    }

    fn current_mut(&mut self) -> &mut CrewAccount {
        &mut self.accounts[self.current]
    }

    /// Accepts the current format, or the single-roster format this file had before accounts
    /// existed (turned into one account, so a saved roster isn't lost).
    fn parse(json: &str) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct SingleRoster {
            places: i32,
            members: Vec<PlannedCrew>,
        }

        let plan = match serde_json::from_str::<CrewPlan>(json) {
            Ok(plan) => plan,
            Err(_) => {
                let old: SingleRoster = serde_json::from_str(json).ok()?;
                CrewPlan {
                    accounts: vec![CrewAccount {
                        name: "Compte 1".to_string(),
                        places: old.places,
                        members: old.members,
                    }],
                    current: 0,
                }
            }
        };
        (!plan.accounts.is_empty() && plan.current < plan.accounts.len()).then_some(plan)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct CrewAccount {
    name: String,
    places: i32,
    members: Vec<PlannedCrew>,
}

impl CrewAccount {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            places: 20,
            members: Vec::new(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct PlannedCrew {
    uid: u64,
    character_design_id: i32,
    level: i32,
    training: [i32; CrewStat::ALL.len()],
    equipment: Vec<PlannedEquipment>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct PlannedEquipment {
    item_design_id: i32,
    /// The per-item random bonus (`BonusEnhancementType`/`Value` in game), if the planned piece
    /// rolled one.
    bonus: Option<PlannedBonus>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct PlannedBonus {
    /// An `EnhancementType` name (`Weapon`, `Attack`...).
    enhancement_type: String,
    value: f64,
}

#[derive(Clone)]
enum PrestigeState {
    Loading,
    Loaded(CharacterPrestiges),
    Failed(String),
}

#[derive(Default)]
pub struct CrewTab {
    pub characters: Vec<CharacterDesignXml>,
    pub equipment: Vec<ItemDesignXml>,
    plan: CrewPlan,
    saved: CrewPlan,
    selected: Option<u64>,
    add_search: String,
    rarity_filter: Option<&'static str>,
    equip_search: HashMap<EquipmentSlot, String>,
    /// Keyed by character design id, filled by a background fetch thread.
    prestige: Arc<Mutex<HashMap<i32, PrestigeState>>>,
    /// `Some` while the current account's name is being edited.
    renaming: Option<String>,
    confirm_delete_account: bool,
}

impl CrewTab {
    pub fn load() -> Self {
        let plan = std::fs::read_to_string(app_paths().crew_plan())
            .ok()
            .and_then(|s| CrewPlan::parse(&s))
            .unwrap_or_default();
        Self {
            selected: plan.current().members.first().map(|m| m.uid),
            saved: plan.clone(),
            plan,
            ..Default::default()
        }
    }

    fn account(&self) -> &CrewAccount {
        self.plan.current()
    }

    fn account_mut(&mut self) -> &mut CrewAccount {
        self.plan.current_mut()
    }

    fn switch_account(&mut self, index: usize) {
        self.plan.current = index;
        self.selected = self.account().members.first().map(|m| m.uid);
        self.renaming = None;
        self.confirm_delete_account = false;
    }

    fn save_if_changed(&mut self) {
        if self.plan == self.saved {
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.plan) {
            let _ = std::fs::write(app_paths().crew_plan(), json);
        }
        self.saved = self.plan.clone();
    }
}

pub fn render(ui: &mut egui::Ui, tab: &mut CrewTab) {
    if tab.characters.is_empty() {
        ui.label(tr!("plan.no_designs"));
        return;
    }

    // Placeholder rows left by a schema migration until the next designs sync (see
    // `DesignsCache::has_invalidated_domains`) - slots/training capacity would read as zero
    if tab
        .characters
        .iter()
        .any(|c| c.equipment_mask < 0 || c.progression_type.is_empty())
    {
        ui.colored_label(palette::WARN, tr!("plan.incomplete_data"));
    }

    ui.horizontal(|ui| {
        render_account_bar(ui, tab);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let min_places = tab.account().members.len() as i32;
            if ui.button("+").clicked() {
                tab.account_mut().places += 1;
            }
            ui.add(
                egui::DragValue::new(&mut tab.account_mut().places).range(min_places.max(1)..=200),
            );
            if ui
                .add_enabled(
                    tab.account().places > min_places.max(1),
                    egui::Button::new("−"),
                )
                .clicked()
            {
                tab.account_mut().places -= 1;
            }
            ui.label(tr!("plan.seats"));
        });
    });
    ui.add_space(6.0);

    ui.columns(3, |columns| {
        render_roster(&mut columns[0], tab);
        render_member_detail(&mut columns[1], tab);
        render_stats(&mut columns[2], tab);
        columns[2].add_space(8.0);
        render_prestige(&mut columns[2], tab);
    });

    tab.save_if_changed();
}

// ---------------------------------------------------------------- accounts

fn render_account_bar(ui: &mut egui::Ui, tab: &mut CrewTab) {
    ui.label(tr!("plan.account"));

    if let Some(name) = tab.renaming.as_mut() {
        let response = ui.add(egui::TextEdit::singleline(name).desired_width(180.0));
        response.request_focus();
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if ui.button(tr!("plan.ok")).clicked() || enter {
            let name = name.trim().to_string();
            if !name.is_empty() {
                tab.account_mut().name = name;
            }
            tab.renaming = None;
        } else if escape {
            tab.renaming = None;
        }
        return;
    }

    let mut switch_to = None;
    egui::ComboBox::from_id_salt("crew_account")
        .selected_text(tr!(
            "plan.account_summary",
            name = tab.account().name.as_str(),
            n = tab.account().members.len()
        ))
        .width(220.0)
        .show_ui(ui, |ui| {
            for (i, account) in tab.plan.accounts.iter().enumerate() {
                let label = tr!(
                    "plan.account_summary",
                    name = account.name.as_str(),
                    n = account.members.len()
                );
                if ui.selectable_label(i == tab.plan.current, label).clicked() {
                    switch_to = Some(i);
                }
            }
        });
    if let Some(index) = switch_to {
        tab.switch_account(index);
    }

    if ui.small_button(tr!("plan.new")).clicked() {
        let name = tr!("plan.account_n", n = tab.plan.accounts.len() + 1);
        tab.plan.accounts.push(CrewAccount::named(&name));
        tab.switch_account(tab.plan.accounts.len() - 1);
        tab.renaming = Some(name);
    }
    if ui.small_button(tr!("plan.rename")).clicked() {
        tab.renaming = Some(tab.account().name.clone());
    }

    let can_delete = tab.plan.accounts.len() > 1;
    if tab.confirm_delete_account {
        ui.colored_label(
            palette::BAD,
            tr!("plan.delete_confirm", name = tab.account().name.as_str()),
        );
        if ui.small_button(tr!("plan.yes")).clicked() {
            let index = tab.plan.current;
            tab.plan.accounts.remove(index);
            tab.switch_account(index.min(tab.plan.accounts.len() - 1));
        }
        if ui.small_button(tr!("plan.no")).clicked() {
            tab.confirm_delete_account = false;
        }
    } else if ui
        .add_enabled(can_delete, egui::Button::new(tr!("plan.delete")).small())
        .on_disabled_hover_text(tr!("plan.keep_one"))
        .clicked()
    {
        tab.confirm_delete_account = true;
    }
}

// ---------------------------------------------------------------- roster

fn render_roster(ui: &mut egui::Ui, tab: &mut CrewTab) {
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.strong(tr!("plan.roster"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!(
                    "{} / {}",
                    tab.account().members.len(),
                    tab.account().places
                ));
            });
        });

        let unknown_design = tr!("plan.unknown_design");
        let mut clicked = None;
        egui::Grid::new("crew_roster_grid")
            .num_columns(2)
            .show(ui, |ui| {
                for (i, member) in tab.account().members.iter().enumerate() {
                    let design = find_character(&tab.characters, member.character_design_id);
                    let (name, rarity) = design
                        .map(|d| (d.character_design_name.as_str(), d.rarity.as_str()))
                        .unwrap_or((unknown_design.as_str(), ""));
                    let text = egui::RichText::new(tr!(
                        "plan.member_line",
                        name = name,
                        level = member.level,
                        rarity = rarity
                    ))
                    .color(rarity_color(rarity));
                    if ui
                        .selectable_label(tab.selected == Some(member.uid), text)
                        .clicked()
                    {
                        clicked = Some(member.uid);
                    }
                    if i % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
        if clicked.is_some() {
            tab.selected = clicked;
        }
        if tab.account().members.is_empty() {
            ui.weak(tr!("plan.roster_empty"));
        }

        ui.separator();
        render_add_member(ui, tab);
    });
}

fn render_add_member(ui: &mut egui::Ui, tab: &mut CrewTab) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut tab.add_search)
                .hint_text(tr!("plan.search_hint"))
                .desired_width(160.0),
        );
        egui::ComboBox::from_id_salt("crew_rarity_filter")
            .selected_text(
                tab.rarity_filter
                    .map_or_else(|| tr!("plan.rarity"), |r| r.to_string()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut tab.rarity_filter, None, tr!("plan.all"));
                for rarity in RARITIES {
                    ui.selectable_value(&mut tab.rarity_filter, Some(rarity), rarity);
                }
            });
    });

    let full = tab.account().members.len() as i32 >= tab.account().places;
    if full {
        ui.colored_label(palette::WARN, tr!("plan.roster_full"));
    }
    if tab.add_search.is_empty() && tab.rarity_filter.is_none() {
        return;
    }

    let needle = tab.add_search.to_lowercase();
    let mut to_add = None;
    egui::ScrollArea::vertical()
        .id_salt("crew_add_results")
        .max_height(240.0)
        .show(ui, |ui| {
            for design in tab
                .characters
                .iter()
                .filter(|d| tab.rarity_filter.is_none_or(|r| d.rarity == r))
                .filter(|d| {
                    needle.is_empty() || d.character_design_name.to_lowercase().contains(&needle)
                })
                .take(MAX_SEARCH_RESULTS)
            {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!full, egui::Button::new("+").small())
                        .clicked()
                    {
                        to_add = Some((design.character_design_id, design.max_character_level));
                    }
                    ui.colored_label(rarity_color(&design.rarity), &design.character_design_name);
                    ui.weak(&design.rarity);
                });
            }
        });

    if let Some((character_design_id, level)) = to_add {
        let uid = tab
            .account()
            .members
            .iter()
            .map(|m| m.uid)
            .max()
            .unwrap_or(0)
            + 1;
        tab.account_mut().members.push(PlannedCrew {
            uid,
            character_design_id,
            level,
            training: [0; CrewStat::ALL.len()],
            equipment: Vec::new(),
        });
        tab.selected = Some(uid);
    }
}

// ---------------------------------------------------------------- member detail

fn render_member_detail(ui: &mut egui::Ui, tab: &mut CrewTab) {
    let CrewTab {
        characters,
        equipment,
        plan,
        selected,
        equip_search,
        ..
    } = tab;
    let plan = plan.current_mut();
    let Some(index) = plan.members.iter().position(|m| Some(m.uid) == *selected) else {
        ui.weak(tr!("plan.select_member"));
        return;
    };
    let Some(design) = find_character(characters, plan.members[index].character_design_id) else {
        ui.weak(tr!("plan.design_missing"));
        return;
    };

    let mut remove = false;
    ui.group(|ui| {
        let member = &mut plan.members[index];
        ui.horizontal(|ui| {
            ui.heading(
                egui::RichText::new(&design.character_design_name)
                    .color(rarity_color(&design.rarity)),
            );
            ui.colored_label(rarity_color(&design.rarity), &design.rarity);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("×")
                    .on_hover_text(tr!("plan.remove"))
                    .clicked()
                {
                    remove = true;
                }
                ui.add(
                    egui::DragValue::new(&mut member.level).range(1..=design.max_character_level),
                );
                ui.label(tr!("plan.lvl"));
            });
        });

        egui::Grid::new("crew_member_info")
            .num_columns(2)
            .show(ui, |ui| {
                ui.weak(tr!("plan.ability"));
                ui.label(tr!(
                    "plan.ability_line",
                    name = ability_display_name(&design.special_ability_type),
                    abl = fmt_stat(training::base_stat(design, CrewStat::Ability, member.level))
                ));
                ui.end_row();
                ui.weak(tr!("plan.slots"));
                let slots = EquipmentSlot::from_mask(design.equipment_mask);
                ui.label(if slots.is_empty() {
                    tr!("plan.none")
                } else {
                    slots
                        .iter()
                        .map(|s| slot_label(*s))
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                ui.end_row();
            });

        ui.separator();
        render_equipment(ui, member, design, equipment, equip_search);
        ui.separator();
        render_training(ui, member, design);
    });

    if remove {
        plan.members.remove(index);
        *selected = plan.members.first().map(|m| m.uid);
    }
}

fn render_equipment(
    ui: &mut egui::Ui,
    member: &mut PlannedCrew,
    design: &CharacterDesignXml,
    catalog: &[ItemDesignXml],
    equip_search: &mut HashMap<EquipmentSlot, String>,
) {
    ui.strong(tr!("plan.equipment"));
    let slots = EquipmentSlot::from_mask(design.equipment_mask);
    if slots.is_empty() {
        ui.weak(tr!("plan.cannot_equip"));
        return;
    }

    for slot in slots {
        let equipped = member.equipment.iter().position(|e| {
            find_item(catalog, e.item_design_id)
                .and_then(|item| EquipmentSlot::from_item_sub_type(&item.item_sub_type))
                == Some(slot)
        });

        ui.push_id(slot_label(slot), |ui| match equipped {
            Some(i) => {
                let mut unequip = false;
                let piece = &mut member.equipment[i];
                ui.horizontal(|ui| {
                    let item = find_item(catalog, piece.item_design_id);
                    match item {
                        Some(it) => {
                            ui.colored_label(rarity_color(&it.rarity), &it.item_design_name)
                        }
                        None => ui.label(tr!("plan.unknown_item")),
                    };
                    ui.colored_label(
                        palette::GOOD,
                        equipment_bonus_text(item, piece.bonus.as_ref()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        unequip = ui.small_button("×").clicked();
                        ui.weak(slot_label(slot));
                    });
                });
                render_bonus_editor(ui, piece);
                if unequip {
                    member.equipment.remove(i);
                }
            }
            None => {
                let query = equip_search.entry(slot).or_default();
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(query)
                            .hint_text(tr!("plan.item_hint"))
                            .desired_width(200.0),
                    );
                    ui.weak(slot_label(slot));
                });
                if !query.is_empty() {
                    let mut chosen = None;
                    for item in catalog
                        .iter()
                        .filter(|it| {
                            EquipmentSlot::from_item_sub_type(&it.item_sub_type) == Some(slot)
                        })
                        .filter(|it| item_matches(it, query))
                        .take(8)
                    {
                        let label = egui::RichText::new(format!(
                            "{}  {}",
                            item.item_design_name,
                            equipment_bonus_text(Some(item), None)
                        ))
                        .color(rarity_color(&item.rarity));
                        if ui.small_button(label).on_hover_text(&item.rarity).clicked() {
                            chosen = Some(item.item_design_id);
                        }
                    }
                    if let Some(item_design_id) = chosen {
                        member.equipment.push(PlannedEquipment {
                            item_design_id,
                            bonus: None,
                        });
                        query.clear();
                    }
                }
            }
        });
    }
}

fn render_bonus_editor(ui: &mut egui::Ui, piece: &mut PlannedEquipment) {
    ui.horizontal(|ui| {
        ui.weak(tr!("plan.bonus"));
        let current = piece
            .bonus
            .as_ref()
            .and_then(|b| CrewStat::from_enhancement(&b.enhancement_type));
        let mut choice = current;
        egui::ComboBox::from_id_salt("bonus_stat")
            .selected_text(choice.map_or_else(|| tr!("plan.none"), |s| stat_abbrev(s).to_string()))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut choice, None, tr!("plan.none"));
                for stat in CrewStat::ALL {
                    ui.selectable_value(&mut choice, Some(stat), stat_abbrev(stat));
                }
            });
        if choice != current {
            piece.bonus = choice.map(|stat| PlannedBonus {
                enhancement_type: stat.enhancement_name().to_string(),
                value: piece.bonus.as_ref().map_or(0.0, |b| b.value),
            });
        }
        if let Some(bonus) = piece.bonus.as_mut() {
            ui.add(
                egui::DragValue::new(&mut bonus.value)
                    .speed(0.1)
                    .range(0.0..=100.0),
            );
        }
    });
}

fn render_training(ui: &mut egui::Ui, member: &mut PlannedCrew, design: &CharacterDesignXml) {
    let mut training = Training::from_points(member.training);
    let capacity = design.training_capacity;
    ui.horizontal(|ui| {
        ui.strong(tr!("plan.training"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(tr!(
                "plan.training_used",
                used = training.total(),
                cap = capacity
            ));
        });
    });
    let fraction = if capacity > 0 {
        training.total() as f32 / capacity as f32
    } else {
        0.0
    };
    ui.add(egui::ProgressBar::new(fraction.min(1.0)).desired_height(6.0));

    egui::Grid::new("crew_training_grid")
        .num_columns(4)
        .show(ui, |ui| {
            for (i, stat) in CrewStat::ALL.into_iter().enumerate() {
                let remaining = capacity - training.total();
                let mut points = training.get(stat);
                let max_points = points + remaining.max(0);
                ui.label(crew_stat_label(stat));
                ui.horizontal(|ui| {
                    if ui.small_button("−").clicked() {
                        points -= 1;
                    }
                    ui.add(egui::DragValue::new(&mut points).range(0..=max_points));
                    if ui
                        .add_enabled(remaining > 0, egui::Button::new("+").small())
                        .clicked()
                    {
                        points += 1;
                    }
                });
                training.set(stat, points);
                if i % 2 == 1 {
                    ui.end_row();
                }
            }
        });

    if ui.button(tr!("plan.reset")).clicked() {
        training = Training::default();
    }
    member.training = training.points();
}

// ---------------------------------------------------------------- resulting stats

fn render_stats(ui: &mut egui::Ui, tab: &CrewTab) {
    let Some(member) = tab
        .account()
        .members
        .iter()
        .find(|m| Some(m.uid) == tab.selected)
    else {
        return;
    };
    let Some(design) = find_character(&tab.characters, member.character_design_id) else {
        return;
    };

    let bonuses: Vec<(CrewStat, f64)> = member
        .equipment
        .iter()
        .flat_map(|piece| {
            let item = find_item(&tab.equipment, piece.item_design_id).and_then(|it| {
                CrewStat::from_enhancement(&it.enhancement_type).map(|s| (s, it.enhancement_value))
            });
            let bonus = piece.bonus.as_ref().and_then(|b| {
                CrewStat::from_enhancement(&b.enhancement_type).map(|s| (s, b.value))
            });
            item.into_iter().chain(bonus)
        })
        .collect();
    let training = Training::from_points(member.training);

    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.strong(tr!("plan.results"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(tr!("plan.scale", lvl = member.level));
            });
        });
        egui::Grid::new("crew_stats_grid")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                ui.weak(tr!("plan.stat"));
                ui.weak(tr!("plan.base"));
                ui.weak(tr!("plan.final"));
                ui.weak("Δ");
                ui.end_row();
                for row in training::crew_stats(design, member.level, &training, &bonuses) {
                    let delta = row.training_gain + row.equipment_bonus;
                    ui.label(crew_stat_label(row.stat));
                    ui.label(fmt_stat(row.base));
                    ui.label(fmt_stat(row.final_value));
                    if delta > 0.0 {
                        ui.colored_label(palette::GOOD, format!("+{}", fmt_stat(delta)));
                    } else {
                        ui.weak("—");
                    }
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
        ui.weak(tr!("plan.footer"));
    });
}

// ---------------------------------------------------------------- prestige

fn request_prestige(tab: &CrewTab, ctx: &egui::Context, character_design_id: i32) {
    let mut states = tab.prestige.lock().unwrap();
    if states.contains_key(&character_design_id) {
        return;
    }
    states.insert(character_design_id, PrestigeState::Loading);

    let store = Arc::clone(&tab.prestige);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| {
                runtime.block_on(prestige::fetch_character_prestiges(
                    &reqwest::Client::new(),
                    character_design_id,
                ))
            });
        let state = match result {
            Ok(prestiges) => PrestigeState::Loaded(prestiges),
            Err(err) => PrestigeState::Failed(format!("{err:#}")),
        };
        store.lock().unwrap().insert(character_design_id, state);
        ctx.request_repaint();
    });
}

fn render_prestige(ui: &mut egui::Ui, tab: &mut CrewTab) {
    let Some(design_id) = tab
        .account()
        .members
        .iter()
        .find(|m| Some(m.uid) == tab.selected)
        .map(|m| m.character_design_id)
    else {
        return;
    };
    request_prestige(tab, ui.ctx(), design_id);
    let state = tab.prestige.lock().unwrap().get(&design_id).cloned();

    let name = |id: i32| {
        find_character(&tab.characters, id)
            .map_or_else(|| format!("#{id}"), |c| c.character_design_name.clone())
    };
    let roster: HashSet<i32> = tab
        .account()
        .members
        .iter()
        .map(|m| m.character_design_id)
        .collect();
    // Ingredients already in the roster are highlighted - they're fusions you can do right away
    let chip = |ui: &mut egui::Ui, id: i32| {
        let text = egui::RichText::new(name(id));
        if roster.contains(&id) {
            ui.colored_label(palette::GOOD, text.strong());
        } else {
            ui.label(text);
        }
    };

    ui.group(|ui| {
        let prestiges = match state {
            Some(PrestigeState::Loaded(prestiges)) => prestiges,
            Some(PrestigeState::Failed(err)) => {
                ui.strong(tr!("prestige.title"));
                ui.colored_label(
                    palette::BAD,
                    tr!("prestige.load_failed", err = err.as_str()),
                );
                if ui.button(tr!("prestige.retry")).clicked() {
                    tab.prestige.lock().unwrap().remove(&design_id);
                }
                return;
            }
            Some(PrestigeState::Loading) | None => {
                ui.strong(tr!("prestige.title"));
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr!("prestige.loading"));
                });
                return;
            }
        };

        let mut recipes: Vec<(i32, i32)> = Vec::new();
        for p in &prestiges.to {
            let pair = (
                p.character_design_id_1.min(p.character_design_id_2),
                p.character_design_id_1.max(p.character_design_id_2),
            );
            if !recipes.contains(&pair) {
                recipes.push(pair);
            }
        }

        ui.horizontal(|ui| {
            ui.strong(tr!("prestige.title"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(tr!(
                    "prestige.counts",
                    recipes = recipes.len(),
                    fusions = prestiges.from.len()
                ));
            });
        });

        let target_name = name(design_id);
        ui.weak(tr!("prestige.combine_to", name = target_name.as_str()));
        if recipes.is_empty() {
            ui.weak(tr!("prestige.no_recipe"));
        }
        for (a, b) in &recipes {
            ui.horizontal_wrapped(|ui| {
                chip(ui, *a);
                ui.weak("+");
                chip(ui, *b);
                ui.weak("→");
                ui.colored_label(palette::WARN, &target_name);
            });
        }

        ui.separator();
        ui.weak(tr!("prestige.combine_with", name = target_name.as_str()));
        if prestiges.from.is_empty() {
            ui.weak(tr!("prestige.no_use"));
        }
        let mut by_target: Vec<(i32, Vec<i32>)> = Vec::new();
        for p in &prestiges.from {
            let partner = p.partner_of(design_id);
            match by_target
                .iter_mut()
                .find(|(t, _)| *t == p.to_character_design_id)
            {
                Some((_, partners)) => partners.push(partner),
                None => by_target.push((p.to_character_design_id, vec![partner])),
            }
        }
        for (target, partners) in &by_target {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(palette::WARN, egui::RichText::new(name(*target)).strong());
                ui.weak("←");
                for (i, partner) in partners.iter().enumerate() {
                    if i > 0 {
                        ui.weak("·");
                    }
                    chip(ui, *partner);
                }
            });
        }

        let usable = prestiges
            .from
            .iter()
            .filter(|p| roster.contains(&p.partner_of(design_id)))
            .count();
        ui.add_space(4.0);
        ui.weak(tr!(
            "prestige.usable",
            usable = usable,
            total = prestiges.from.len()
        ));
    });
}

// ---------------------------------------------------------------- helpers

fn find_character(characters: &[CharacterDesignXml], id: i32) -> Option<&CharacterDesignXml> {
    characters.iter().find(|c| c.character_design_id == id)
}

fn find_item(catalog: &[ItemDesignXml], id: i32) -> Option<&ItemDesignXml> {
    catalog.iter().find(|it| it.item_design_id == id)
}

/// Plain substring on the name, or `+ABBR` to search by boosted stat (e.g. `+RPR`).
fn item_matches(item: &ItemDesignXml, query: &str) -> bool {
    match query.strip_prefix('+') {
        Some(abbrev) => CrewStat::from_enhancement(&item.enhancement_type)
            .is_some_and(|stat| stat_abbrev(stat).eq_ignore_ascii_case(abbrev.trim())),
        None => item
            .item_design_name
            .to_lowercase()
            .contains(&query.to_lowercase()),
    }
}

/// "+6 WPN · +1.2 ATK": the design's fixed enhancement, then the planned random bonus.
fn equipment_bonus_text(item: Option<&ItemDesignXml>, bonus: Option<&PlannedBonus>) -> String {
    let mut parts = Vec::new();
    if let Some(item) = item.filter(|it| it.enhancement_type != "None") {
        parts.push(format!(
            "+{} {}",
            fmt_stat(item.enhancement_value),
            enhancement_abbrev(&item.enhancement_type)
        ));
    }
    if let Some(bonus) = bonus {
        parts.push(format!(
            "+{} {}",
            fmt_stat(bonus.value),
            enhancement_abbrev(&bonus.enhancement_type)
        ));
    }
    parts.join(" · ")
}

fn enhancement_abbrev(kind: &str) -> String {
    match CrewStat::from_enhancement(kind) {
        Some(stat) => stat_abbrev(stat).to_string(),
        None if kind == "FireResistance" => "FIRE".to_string(),
        None => kind.to_string(),
    }
}

fn stat_abbrev(stat: CrewStat) -> &'static str {
    match stat {
        CrewStat::Hp => "HP",
        CrewStat::Attack => "ATK",
        CrewStat::Repair => "RPR",
        CrewStat::Ability => "ABL",
        CrewStat::Stamina => "STA",
        CrewStat::Pilot => "PLT",
        CrewStat::Science => "SCI",
        CrewStat::Engine => "ENG",
        CrewStat::Weapon => "WPN",
    }
}

fn crew_stat_label(stat: CrewStat) -> String {
    match stat {
        CrewStat::Hp => tr!("stat.hp"),
        CrewStat::Attack => tr!("stat.attack"),
        CrewStat::Repair => tr!("stat.repair"),
        CrewStat::Ability => tr!("stat.ability"),
        CrewStat::Stamina => tr!("stat.stamina"),
        CrewStat::Pilot => tr!("stat.pilot"),
        CrewStat::Science => tr!("stat.science"),
        CrewStat::Engine => tr!("stat.engine"),
        CrewStat::Weapon => tr!("stat.weapon"),
    }
}

fn slot_label(slot: EquipmentSlot) -> &'static str {
    match slot {
        EquipmentSlot::Head => "Head",
        EquipmentSlot::Body => "Body",
        EquipmentSlot::Leg => "Leg",
        EquipmentSlot::Weapon => "Weapon",
        EquipmentSlot::Accessory => "Accessory",
        EquipmentSlot::Pet => "Pet",
    }
}

/// One color per rarity, from PixyShip's frontend (frontend/src/assets/css/common.css).
fn rarity_color(rarity: &str) -> egui::Color32 {
    match rarity {
        "Common" => egui::Color32::WHITE,
        "Elite" => egui::Color32::from_rgb(0x1b, 0xe6, 0x00),
        "Unique" => egui::Color32::from_rgb(0x07, 0x85, 0xff),
        "Epic" => egui::Color32::from_rgb(0xba, 0x54, 0xff),
        "Hero" => egui::Color32::from_rgb(0xff, 0x80, 0x00),
        "Special" => egui::Color32::from_rgb(0xfe, 0xfc, 0x89),
        "Legendary" => egui::Color32::from_rgb(0xd5, 0xaa, 0x2a),
        _ => palette::NEUTRAL,
    }
}

/// Up to 2 decimals, trailing zeros dropped ("2.20" -> "2.2", "9.00" -> "9").
fn fmt_stat(value: f64) -> String {
    let s = format!("{value:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The design stores its ability as the enum's variant name (e.g. "AddReload").
fn ability_display_name(name: &str) -> String {
    (0..64)
        .map(SpecialAbilityType::from)
        .find(|ability| format!("{ability:?}") == name)
        .map(special_ability_display_name)
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_roster_file_becomes_the_first_account() {
        let old = r#"{"places":23,"members":[{"uid":1,"character_design_id":583,"level":40,
            "training":[0,0,0,0,0,0,0,0,10],"equipment":[]}]}"#;
        let plan = CrewPlan::parse(old).expect("old format should still load");
        assert_eq!(plan.accounts.len(), 1);
        assert_eq!(plan.current().places, 23);
        assert_eq!(plan.current().members[0].character_design_id, 583);
    }

    #[test]
    fn multi_account_file_round_trips() {
        let mut plan = CrewPlan::default();
        plan.accounts.push(CrewAccount::named("Alt"));
        plan.current = 1;
        let json = serde_json::to_string(&plan).unwrap();
        assert!(CrewPlan::parse(&json) == Some(plan));
    }

    #[test]
    fn out_of_range_current_account_is_rejected() {
        assert!(CrewPlan::parse(r#"{"accounts":[],"current":0}"#).is_none());
    }
}
