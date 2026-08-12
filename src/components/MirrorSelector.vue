<template>
  <el-select
    v-model="selectedMirror"
    placeholder="选择镜像源"
    :disabled="disabled"
    @change="handleChange"
    class="mirror-selector"
  >
    <el-option
      v-for="mirror in mirrors"
      :key="mirror.id"
      :label="mirror.name"
      :value="mirror.registry"
    >
      <div class="mirror-option">
        <span>{{ mirror.name }}</span>
        <span v-if="mirror.isDefault" class="default-badge">默认</span>
      </div>
    </el-option>
  </el-select>
</template>

<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { useSettingsStore } from '@/stores/settings'
import type { Mirror } from '@/types/api'

interface Props {
  modelValue: string
  disabled?: boolean
}

const props = withDefaults(defineProps<Props>(), {
  modelValue: '',
  disabled: false
})

const emit = defineEmits<{
  (e: 'update:modelValue', value: string): void
}>()

const settingsStore = useSettingsStore()
const mirrors = computed<Mirror[]>(() => settingsStore.mirrors)

const selectedMirror = ref<string>(props.modelValue || settingsStore.getDefaultMirrorRegistry())

watch(() => props.modelValue, (val: string) => {
  if (val) {
    selectedMirror.value = val
  }
})

const handleChange = (value: string): void => {
  emit('update:modelValue', value)
}
</script>

<style scoped>
.mirror-selector {
  width: 100%;
}
.mirror-option {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
.default-badge {
  background: #409eff;
  color: #fff;
  padding: 2px 6px;
  border-radius: 4px;
  font-size: 12px;
}
</style>
