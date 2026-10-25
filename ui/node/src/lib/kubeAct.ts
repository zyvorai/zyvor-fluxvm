import { api } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { type KubeVerb, actionToast } from './kube';

/** POSTs a cluster action and tells the user the outcome either way; true when the cluster accepted it. */
export async function kubeAct(path: string, subject: string, verb: KubeVerb, notify: Notify, body?: unknown, replicas?: number): Promise<boolean> {
  try {
    await api.post(path, body);
    notify(actionToast(subject, verb, replicas));
    return true;
  } catch (e) {
    notify(`${subject}: ${(e as Error).message}`, 'bad');
    return false;
  }
}
