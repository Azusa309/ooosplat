import type { TaskDraft } from './taskWorkspace';
import type { VideoProjection } from '../types/pipeline';

export type PreflightKind = 'smallImages' | 'largeImages';
export interface PreflightQuestion { id: string; kind: PreflightKind; draft: TaskDraft; snapshot: string }
export type PreflightAnswers = Record<string, string>;
export const projectionIdentity = (draft: TaskDraft) => draft.projectionDetection?.sourceIdentity || draft.inputPath || '';
export const preflightSnapshot = (draft: TaskDraft) => JSON.stringify([draft.id, draft.inputPath, draft.inputType, draft.quality, draft.plannerEnabled, projectionIdentity(draft), draft.imageSequence?.imageCount, draft.projectionDetection?.confirmedErp, draft.projectionDetection?.supportedErp, resolvedProjection(draft)]);
export function resolvedProjection(draft: TaskDraft): VideoProjection {
  const detection = draft.projectionDetection;
  if (detection?.confirmedErp) return 'equirectangular';
  const answer = draft.projectionAnswer;
  if (answer?.identity === projectionIdentity(draft) && (answer.projection !== 'equirectangular' || detection?.supportedErp)) return answer.projection;
  return 'auto';
}
// Ordered registry: add another check without coupling rendering to task startup.
const checks: Array<{ kind: PreflightKind; applies: (draft: TaskDraft) => boolean }> = [
  { kind: 'smallImages', applies: d => d.inputType === 'images' && !!d.imageSequence && d.imageSequence.imageCount < 30 },
  { kind: 'largeImages', applies: d => d.inputType === 'images' && !!d.imageSequence?.requiresLargeSequenceConfirmation },
];
export function collectPreflightQuestions(drafts: TaskDraft[]): PreflightQuestion[] {
  return drafts.filter(d => !d.running && !!d.inputPath).flatMap(draft => checks
    .filter(check => check.applies(draft))
    .map(check => ({ id: `${draft.id}:${check.kind}`, kind: check.kind, draft, snapshot: preflightSnapshot(draft) })));
}
export function answerIsValid(_question: PreflightQuestion, answer?: string) { return answer === 'accepted'; }
