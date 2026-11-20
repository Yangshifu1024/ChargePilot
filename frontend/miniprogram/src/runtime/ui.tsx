import { ReactNode } from 'react'
import { Button, ScrollView, Text, View } from '@tarojs/components'
import Taro from '@tarojs/taro'
import { AtIcon } from 'taro-ui'
import { isH5, wx } from './platform'

/** 次级页返回原页面；无上级页面时回到首页，不留下失效返回入口。 */
function back() {
  if (Taro.getCurrentPages().length > 1) wx.navigateBack({ delta: 1 })
  else wx.reLaunch({ url: '/pages/index/index' })
}

const GREEN = ['charging', 'completed', 'finished', 'paid', 'idle', 'enabled', 'active', 'unused', 'done']
const AMBER = ['pending_payment', 'refunding', 'reserved', 'confirming', 'manual_review', 'processing']
const RED = ['failed', 'fault', 'cancelled', 'disabled', 'lost']
const GREY = ['refunded', 'expired', 'used', 'offline', 'retired', 'full']

/** 状态色只作辅助，文案仍由调用方展示。 */
export function statusTone(status?: string) {
  if (!status) return 'tag-grey'
  if (GREEN.includes(status)) return 'tag-green'
  if (AMBER.includes(status)) return 'tag-orange'
  if (RED.includes(status)) return 'tag-red'
  if (GREY.includes(status)) return 'tag-grey'
  return 'tag-blue'
}

/** 页头展示业务名称；H5 补充返回按钮，微信端复用原生导航。 */
export function PageHeader({ title, subtitle, root = false }: { title: string; subtitle?: string; root?: boolean }) {
  return <View className='page-header'>
    {isH5 && !root && <Button className='back-button' aria-label='返回' onClick={back}><AtIcon value='chevron-left' size={22} /></Button>}
    <View className='page-header-copy'>
      <Text className='page-title'>{title}</Text>
      {subtitle && <Text className='page-subtitle'>{subtitle}</Text>}
    </View>
  </View>
}

/** 通用提示保留错误与加载语义，重试由页面控制器处理。 */
export function Notice({ children, retry, busy = false, tone = '' }: { children: ReactNode; retry?: (event: any) => void; busy?: boolean; tone?: string }) {
  return <View className={`card notice ${tone}`}><View>{children}</View>{retry && <Button size='mini' onClick={retry} disabled={busy}>重试</Button>}</View>
}

/** 横向筛选沿用控制器索引，较多状态可横向滚动，不新增查询值。 */
export function Segments({ labels, selected, onSelect }: { labels: string[]; selected: number; onSelect: (index: number) => void }) {
  return <ScrollView scrollX className='segments-scroll'><View className='segments'>{labels.map((label, index) =>
    <Button key={label} className={`segment ${selected === index ? 'is-selected' : ''}`} onClick={() => onSelect(index)}>{label}</Button>
  )}</View></ScrollView>
}

export function LoginCard({ title, detail, action, busy = false, onLogin }: { title: string; detail?: string; action: string; busy?: boolean; onLogin: (event: any) => void }) {
  return <View className='hero'><View className='hero-body'>
    <Text className='eyebrow'>需要登录</Text>
    <Text className='hero-title'>{title}</Text>
    {detail && <Text className='hero-desc'>{detail}</Text>}
    <Button type='primary' loading={busy} disabled={busy} onClick={onLogin}>{action}</Button>
  </View></View>
}

export function Empty({ icon = 'file-generic', children }: { icon?: string; children: ReactNode }) {
  return <View className='card empty'><View className='empty-mark'><AtIcon value={icon} size={22} color='#0e6b52' /></View><Text className='empty-text'>{children}</Text></View>
}

export function Kv({ label, children }: { label: string; children: ReactNode }) {
  return <View className='kv'><Text className='kv-label'>{label}</Text><View className='kv-value'>{children}</View></View>
}

export function CouponTicket({ name, discount, typeText, minimumText, expiresText, statusText, muted = false }: {
  name?: string; discount?: string; typeText?: string; minimumText?: string; expiresText?: string; statusText?: string; muted?: boolean
}) {
  return <View className={`ticket${muted ? ' is-muted' : ''}`}>
    <View className='ticket-stub'><Text className='ticket-discount'>{discount}</Text><Text className='ticket-type'>{typeText}</Text></View>
    <View className='ticket-body'>
      <Text className='ticket-name'>{name}</Text>
      <Text className='text-secondary'>满 ¥{minimumText} 可用</Text>
      <Text className='text-secondary'>有效期至 {expiresText}</Text>
      {statusText && <Text className='tag'>{statusText}</Text>}
    </View>
  </View>
}
