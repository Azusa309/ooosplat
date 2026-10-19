// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { LanguageProvider } from '../i18n';
import { createGenerationDraft } from '../app/taskWorkspace';
import { collectPreflightQuestions } from '../app/generationPreflight';
import { GenerationPreflightDialog } from './GenerationPreflightDialog';
let root: Root, container: HTMLDivElement;
const finish = vi.fn();
const draft = { ...createGenerationDraft(1), inputType: 'images' as const, inputPath: 'E:/small-images', imageSequence: { imageCount: 20, width: 100, height: 100, hasAlpha: false, requiresLargeSequenceConfirmation: false } };
const second = { ...draft, id: 'second', inputPath: 'E:/large-images', imageSequence: { ...draft.imageSequence, imageCount: 600, requiresLargeSequenceConfirmation: true } };
const button = () => container.querySelector<HTMLButtonElement>('.primary-action')!;
beforeEach(() => { Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true }); localStorage.setItem('ooo-splat-language', 'zh-CN'); finish.mockReset(); container = document.createElement('div'); document.body.append(container); root = createRoot(container); });
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });
async function render() { await act(async () => root.render(<LanguageProvider><GenerationPreflightDialog session={{ questions: collectPreflightQuestions([draft, second]), finalLabel: '开始生成', resolve: finish }} onFinish={finish} /></LanguageProvider>)); }
it('requires every answer, supports back navigation and submits all answers once', async () => {
  await render(); expect(button().disabled).toBe(true);
  expect(container.querySelector('img')).toBeNull();
  await act(async () => container.querySelector<HTMLInputElement>('input')!.click());
  await act(async () => button().click());
  expect(container.textContent).toContain('2/2'); expect(button().disabled).toBe(true);
  await act(async () => Array.from(container.querySelectorAll('button')).find(node => node.textContent === '上一步')!.click());
  expect(container.querySelector<HTMLInputElement>('input')!.checked).toBe(true);
  await act(async () => button().click());
  await act(async () => container.querySelector<HTMLInputElement>('input')!.click());
  await act(async () => { button().click(); button().click(); });
  expect(finish).toHaveBeenCalledTimes(1);
  expect(finish.mock.calls[0][0]).toEqual({ [`${draft.id}:smallImages`]: 'accepted', 'second:largeImages': 'accepted' });
});
it('Esc cancels without submitting any partial answer and traps keyboard focus', async () => {
  await render();
  const dialog = container.querySelector<HTMLElement>('[role="dialog"]')!;
  await act(async () => dialog.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true })));
  expect(document.activeElement).toBe(container.querySelector('footer > button'));
  await act(async () => document.activeElement!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true })));
  expect(document.activeElement).toBe(container.querySelector('header button'));
  await act(async () => container.querySelector<HTMLInputElement>('input')!.click());
  const cancel = container.querySelector<HTMLButtonElement>('footer > button')!;
  cancel.focus();
  await act(async () => cancel.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })));
  expect(finish).toHaveBeenCalledWith(null);
});
