//! Explicit-save template editor. Built-in edits are local overrides; restoring
//! the original never modifies embedded templates or previously generated notes.
use super::state::SettingsCache;
use crate::{app_state::AppServices, runtime::Io};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::{
    ActiveTheme, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    label::Label,
    setting::{SettingGroup, SettingItem, SettingPage},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::database::repositories::setting::{
    KEY_DEFAULT_SUMMARY_TEMPLATE, SettingsRepository,
};
use parley_core::events::EventSinkExt;
use parley_core::summary::templates::manager::{TemplateEntry, TemplateStore};
use parley_core::summary::templates::{Template, TemplateSection};

pub fn page(editor: &Entity<TemplateManager>) -> SettingPage {
    let editor = editor.clone();
    SettingPage::new("Templates")
        .icon(Icon::new(gpui_kit::assets::IconName::FileText))
        .description("Create your own meeting notes. Template instructions and summary output language are independent.")
        .group(SettingGroup::new().title("Meeting templates")
            .item(SettingItem::render(move |_, _, _| editor.clone())))
}

struct SectionEditor {
    title: Entity<InputState>,
    instruction: Entity<TextareaState>,
    format: String,
    hint: Entity<InputState>,
}

pub struct TemplateManager {
    store: Result<TemplateStore, String>,
    entries: Vec<TemplateEntry>,
    id: String,
    name: Entity<InputState>,
    description: Entity<InputState>,
    sections: Vec<SectionEditor>,
    dirty: bool,
    busy: bool,
    confirm_remove: bool,
    error: Option<String>,
    message: String,
    _subscriptions: Vec<Subscription>,
}

impl TemplateManager {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = TemplateStore::configured();
        let loaded = store.as_ref().map_err(Clone::clone).and_then(|s| s.list());
        let entries = loaded.as_ref().cloned().unwrap_or_default();
        let name = cx.new(|cx| InputState::new(window, cx));
        let description = cx.new(|cx| InputState::new(window, cx));
        let mut this = Self {
            store,
            entries,
            id: "standard_meeting".into(),
            name,
            description,
            sections: Vec::new(),
            dirty: false,
            busy: false,
            confirm_remove: false,
            error: loaded.err(),
            message: String::new(),
            _subscriptions: Vec::new(),
        };
        this.select("standard_meeting", window, cx);
        this
    }

    fn input(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(text.to_string(), window, cx);
            state
        });
        self._subscriptions
            .push(cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.dirty = true;
                    this.message.clear();
                    cx.notify();
                }
            }));
        input
    }

    fn section(
        &mut self,
        section: &TemplateSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SectionEditor {
        let title = self.input(&section.title, window, cx);
        let hint = self.input(
            section
                .item_format
                .as_deref()
                .or(section.example_item_format.as_deref())
                .unwrap_or(""),
            window,
            cx,
        );
        let instruction = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).rows(3);
            state.set_value(section.instruction.clone(), window, cx);
            state
        });
        self._subscriptions.push(
            cx.subscribe(&instruction, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.dirty = true;
                    this.message.clear();
                    cx.notify();
                }
            }),
        );
        SectionEditor {
            title,
            instruction,
            hint,
            format: section.format.clone(),
        }
    }

    fn apply(
        &mut self,
        id: String,
        template: Template,
        dirty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._subscriptions.clear();
        self.id = id;
        self.name = self.input(&template.name, window, cx);
        self.description = self.input(&template.description, window, cx);
        self.sections = template
            .sections
            .iter()
            .map(|s| self.section(s, window, cx))
            .collect();
        self.dirty = dirty;
        self.confirm_remove = false;
        cx.notify();
    }

    fn select(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .store
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|s| s.get(id))
        {
            Ok(entry) => {
                self.error = None;
                self.message.clear();
                self.apply(id.into(), entry.template, false, window, cx);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn draft(&self, cx: &App) -> Template {
        Template {
            name: self.name.read(cx).value().trim().to_string(),
            description: self.description.read(cx).value().trim().to_string(),
            sections: self
                .sections
                .iter()
                .map(|s| {
                    let hint = s.hint.read(cx).value().trim().to_string();
                    TemplateSection {
                        title: s.title.read(cx).value().trim().to_string(),
                        instruction: s.instruction.read(cx).value().trim().to_string(),
                        format: s.format.clone(),
                        item_format: (!hint.is_empty()).then_some(hint),
                        example_item_format: None,
                    }
                })
                .collect(),
        }
    }

    fn create(&mut self, duplicate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut template = if duplicate {
            self.draft(cx)
        } else {
            Template {
                name: "New template".into(),
                description: "Custom meeting notes.".into(),
                sections: vec![blank_section()],
            }
        };
        if duplicate {
            template.name.push_str(" (copy)");
        }
        self.error = None;
        self.message.clear();
        self.apply(
            parley_core::summary::templates::manager::new_template_id(),
            template,
            true,
            window,
            cx,
        );
    }

    fn persist(&mut self, remove: bool, cx: &mut Context<Self>) {
        let store = match &self.store {
            Ok(s) => s.clone(),
            Err(e) => {
                self.error = Some(e.clone());
                return;
            }
        };
        let template = self.draft(cx);
        if !remove {
            if let Err(e) = template.validate() {
                self.error = Some(e);
                cx.notify();
                return;
            }
        }
        let id = self.id.clone();
        let builtin = self.entries.iter().any(|e| e.id == id && e.built_in);
        self.busy = true;
        self.error = None;
        cx.notify();
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let selected = if remove && !builtin {
                "standard_meeting".to_string()
            } else {
                id.clone()
            };
            let result = io
                .spawn(async move {
                    if remove {
                        store.remove(&id)?;
                    } else {
                        store.save(&id, &template)?;
                    }
                    store.list()
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(entries)) => {
                        this.entries = entries;
                        let cache = cx.global_mut::<SettingsCache>();
                        cache.templates = this
                            .entries
                            .iter()
                            .map(|e| {
                                (
                                    e.id.clone(),
                                    e.template.name.clone(),
                                    e.template.description.clone(),
                                )
                            })
                            .collect();
                        if !cache
                            .templates
                            .iter()
                            .any(|(id, _, _)| *id == cache.default_template_id)
                        {
                            cache.default_template_id = "standard_meeting".into();
                            if let Some(pool) = AppServices::global(cx).pool() {
                                Io::global(cx).spawn(async move {
                                    if let Err(error) = SettingsRepository::set_setting(
                                        &pool,
                                        KEY_DEFAULT_SUMMARY_TEMPLATE,
                                        &"standard_meeting",
                                    )
                                    .await
                                    {
                                        log::warn!("Failed to reset default template: {error}");
                                    }
                                });
                            }
                        }
                        this.select(&selected, window, cx);
                        this.message = if remove {
                            "Template removed or restored."
                        } else {
                            "Template saved."
                        }
                        .into();
                        let _ = AppServices::global(cx)
                            .sink
                            .emit_event("summary-templates-changed", &());
                    }
                    Ok(Err(e)) => this.error = Some(e),
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn make_default(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let id = self.id.clone();
        self.busy = true;
        self.error = None;
        cx.notify();
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let saved_id = id.clone();
            let result = io
                .spawn(async move {
                    SettingsRepository::set_setting(&pool, KEY_DEFAULT_SUMMARY_TEMPLATE, &saved_id)
                        .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(())) => {
                        cx.global_mut::<SettingsCache>().default_template_id = id;
                        this.message = "Default template updated.".into();
                        let _ = AppServices::global(cx)
                            .sink
                            .emit_event("summary-templates-changed", &());
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn blank_section() -> TemplateSection {
    TemplateSection {
        title: "Summary".into(),
        instruction: "Summarize the main outcomes of the meeting.".into(),
        format: "paragraph".into(),
        item_format: None,
        example_item_format: None,
    }
}

impl Render for TemplateManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locked = self.busy || self.dirty;
        let entry = self.entries.iter().find(|e| e.id == self.id);
        let builtin = entry.is_some_and(|e| e.built_in);
        let removable = entry.is_some_and(|e| !e.built_in || e.overridden);
        let default = cx.global::<SettingsCache>().default_template_id == self.id;
        let mut list = h_flex().gap_2().flex_wrap();
        for entry in &self.entries {
            let id = entry.id.clone();
            let label = format!(
                "{}{}",
                entry.template.name,
                if entry.overridden && entry.built_in {
                    " · Customized"
                } else {
                    ""
                }
            );
            list = list.child(
                Button::new(SharedString::from(format!("template-{}", entry.id)))
                    .small()
                    .outline()
                    .when(entry.id == self.id, |button| button.primary())
                    .label(label)
                    .disabled(locked)
                    .on_click(cx.listener(move |this, _, window, cx| this.select(&id, window, cx))),
            );
        }
        let mut content = v_flex()
            .w_full()
            .gap_3()
            .child(list)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("new-template")
                            .small()
                            .outline()
                            .label("New template")
                            .disabled(locked)
                            .on_click(cx.listener(|this, _, w, cx| this.create(false, w, cx))),
                    )
                    .child(
                        Button::new("duplicate-template")
                            .small()
                            .outline()
                            .label("Duplicate")
                            .disabled(locked)
                            .on_click(cx.listener(|this, _, w, cx| this.create(true, w, cx))),
                    )
                    .child(
                        Button::new("save-template-top")
                            .small()
                            .primary()
                            .label("Save template")
                            .disabled(self.busy || !self.dirty)
                            .on_click(cx.listener(|this, _, _, cx| this.persist(false, cx))),
                    )
                    .child(
                        Button::new("discard-template-top")
                            .small()
                            .outline()
                            .label("Discard")
                            .disabled(self.busy || !self.dirty)
                            .on_click(cx.listener(|this, _, w, cx| {
                                let id = if this.entries.iter().any(|e| e.id == this.id) {
                                    this.id.clone()
                                } else {
                                    "standard_meeting".into()
                                };
                                this.select(&id, w, cx);
                            })),
                    ),
            )
            .child(
                Label::new(if self.dirty {
                    "Unsaved changes — save or discard before selecting another template."
                } else if builtin {
                    "Built-in template. Saving creates your own editable override."
                } else {
                    "Custom template, stored on this computer."
                })
                .text_sm(),
            )
            .child(Label::new("Template name"))
            .child(Input::new(&self.name).disabled(self.busy))
            .child(Label::new("Description"))
            .child(Input::new(&self.description).disabled(self.busy));
        let count = self.sections.len();
        for (index, section) in self.sections.iter().enumerate() {
            content = content.child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(format!("Section {}", index + 1)))
                            .child(
                                Button::new(("section-up", index))
                                    .small()
                                    .ghost()
                                    .label("↑")
                                    .disabled(self.busy || index == 0)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.sections.swap(index, index - 1);
                                        this.dirty = true;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(("section-down", index))
                                    .small()
                                    .ghost()
                                    .label("↓")
                                    .disabled(self.busy || index + 1 == count)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.sections.swap(index, index + 1);
                                        this.dirty = true;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(("remove-section", index))
                                    .small()
                                    .ghost()
                                    .label("Remove")
                                    .disabled(self.busy || count == 1)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.sections.remove(index);
                                        this.dirty = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(Input::new(&section.title).disabled(self.busy))
                    .child(Label::new("What should this section include?").text_sm())
                    .child(Textarea::new(&section.instruction).disabled(self.busy))
                    .child(
                        Button::new(("section-format", index))
                            .small()
                            .outline()
                            .label(match section.format.as_str() {
                                "list" => "Format: Bullet list",
                                "string" => "Format: Short text",
                                _ => "Format: Paragraph",
                            })
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let format = &mut this.sections[index].format;
                                *format = match format.as_str() {
                                    "paragraph" => "list",
                                    "list" => "string",
                                    _ => "paragraph",
                                }
                                .into();
                                this.dirty = true;
                                cx.notify();
                            })),
                    )
                    .child(Label::new("Item format hint (optional)").text_sm())
                    .child(Input::new(&section.hint).disabled(self.busy)),
            );
        }
        content = content.child(
            Button::new("add-template-section")
                .small()
                .outline()
                .label("Add section")
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, window, cx| {
                    let section = this.section(&blank_section(), window, cx);
                    this.sections.push(section);
                    this.dirty = true;
                    cx.notify();
                })),
        );
        let mut preview = v_flex()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .child(Label::new("Layout preview").font_semibold())
            .child(
                Label::new(
                    "Section order; generated reports can also include tasks and open questions.",
                )
                .text_sm(),
            )
            .child(Label::new("Meeting title"));
        for section in &self.sections {
            preview = preview.child(Label::new(section.title.read(cx).value()).font_semibold());
        }
        content = content.child(preview).child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new("save-template")
                        .primary()
                        .label(if self.busy {
                            "Saving…"
                        } else {
                            "Save template"
                        })
                        .disabled(self.busy || !self.dirty)
                        .on_click(cx.listener(|this, _, _, cx| this.persist(false, cx))),
                )
                .child(
                    Button::new("discard-template")
                        .outline()
                        .label("Discard changes")
                        .disabled(self.busy || !self.dirty)
                        .on_click(cx.listener(|this, _, w, cx| {
                            let id = if this.entries.iter().any(|e| e.id == this.id) {
                                this.id.clone()
                            } else {
                                "standard_meeting".into()
                            };
                            this.select(&id, w, cx);
                        })),
                )
                .child(
                    Button::new("default-template")
                        .outline()
                        .label(if default {
                            "Default template"
                        } else {
                            "Use as default"
                        })
                        .disabled(locked || default)
                        .on_click(cx.listener(|this, _, _, cx| this.make_default(cx))),
                )
                .child(
                    Button::new("remove-template")
                        .ghost()
                        .label(if builtin {
                            "Restore built-in"
                        } else {
                            "Delete template"
                        })
                        .disabled(self.busy || !removable)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_remove = true;
                            cx.notify();
                        })),
                ),
        );
        if self.confirm_remove {
            content = content.child(
                h_flex()
                    .gap_2()
                    .child(Label::new(
                        "Discard edits and remove this saved template or override?",
                    ))
                    .child(
                        Button::new("confirm-template-removal")
                            .outline()
                            .label("Confirm")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.persist(true, cx))),
                    )
                    .child(
                        Button::new("cancel-template-removal")
                            .ghost()
                            .label("Cancel")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_remove = false;
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(error) = &self.error {
            content = content.child(Label::new(error.clone()).text_color(cx.theme().danger));
        }
        if !self.message.is_empty() {
            content = content.child(Label::new(self.message.clone()));
        }
        content
    }
}
